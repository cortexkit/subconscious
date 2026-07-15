# Adversarial Design Review — subc Federation Phase 3 (DRAFT v1) — Council Synthesis

**Question**: Adversarial gap/blocker hunt on `docs/subc-federation-phase3-design.md`, verifying inheritance claims against phase-2 (`docs/subc-federation-design.md` v4.1). Axes: SECURITY, CORRECTNESS, OPERATIONAL + the five §10 open questions.

**Council**: 7 members — Opus 4.8, GPT 5.4 high, GPT 5.5 xhigh, XAI Composer 2.5, Ollama Minimax M3, Ollama GLM 5.2, Gemini Flash 3.5 high. All 7 returned complete, evidence-backed reviews.

**Convergence**: Unusually high. Seven of the top findings were named by all or nearly all members, with near-identical fixes. The disagreement is almost entirely about *severity ranking* (BLOCKER vs HIGH), not about the existence of the gaps. Verdict split: 3× GO-WITH-CHANGES, 3× NO-GO, 1× "GO-WITH-CHANGES (NO-GO until blockers fixed)". These are the *same verdict* stated with different labels — every member agrees the architecture is sound and every member agrees a specific set of must-fix items gates the build.

**Headline**: The trust core (Noise IK E2E + verify-code ceremony + default-deny verified-gate) is correctly inherited and survives adversarial scrutiny — **no member found a real cross-account routability escalation that defeats the ceremony** (with one important caveat: Gemini's cloud-authored-profile finding, below). The failures are concentrated in the **implementation of the locked decisions** — exactly what is in scope — and in a recurring documentation anti-pattern: **three NEW phase-3 mechanisms are asserted as INHERITED phase-2 properties**, which masks untested surfaces from scrutiny.

---

## Findings (grouped by confidence)

#### #1: Pipe-token semantics are contradictory and redemption authz is unspecified (cross-device / cross-pipe redemption)
- **Severity**: Critical
- **Confidence**: Unanimous (7 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, GLM 5.2, Gemini
- **Issue**: §5.3 shows a SINGLE `pipe_token` in `relay_grant {relay_url, pipe_id, pipe_token}` "issued to both sides"; §7 says "**per-side** `pipe_token`, single-use, short TTL." These directly contradict. Neither says what a token is bound to, where single-use is enforced, whether device X can redeem device Y's grant, or how the RelayDO ties a redeeming WebSocket to an authenticated device identity.
- **Evidence**: §5.3 line 153 (singular `pipe_token`); §7 lines 199-200 (per-side). Phase-2 §5.5 requires "auth-before-resource (a connection proves device identity before any forwarding state is allocated)" — a stricter property phase-3 does not carry forward.
- **Impact**: If the token is symmetric, a token leak or a stolen device token lets an attacker occupy both pipe slots (DoS), masquerade as the peer, or MITM-position on the relay. Cross-device redemption is the exact metadata→routability escalation the threat model claims is blocked. **Ships a security hole.**
- **Fix Direction**: Commit to **per-side tokens**. Bind each token to `(pipe_id, side, device_pubkey, exp, nonce)` — e.g. `pipe_token = HMAC(pipe_id_key, pipe_id || device_pubkey || side || exp || nonce)`. RelayDO atomically check-and-consumes each token exactly once inside its single-threaded WS-upgrade handler (DO serialization kills the TOCTOU), rejects a token redeemed for the wrong side or by the wrong device identity, and binds the redeeming WS to the authenticated device identity (the device token presented at upgrade, or the Noise static key). On idle teardown, retire the old `pipe_id` and mint a NEW `pipe_id` + new tokens. Fix the §5.3 schema to match §7.

#### #2: Candidate racing — "pubkey tie-break as phase 2" is a FALSE inheritance claim; double-win (two carriers) is unresolved
- **Severity**: Critical
- **Confidence**: Unanimous (7 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, GLM 5.2, Gemini
- **Issue**: §5.4 says the initiator is chosen by "pubkey tie-break as phase 2." **Phase-2 v4.1 contains no such tie-break** — grep for `tie-break|initiator|candidate|dial|first-success|simultaneous` returns zero relevant hits. Phase-2's model is *asymmetric by reachability* (reachable side is server, NAT'd side dials — Fork 3, phase-2 line 242). Phase-3 makes discovery *symmetric*: both devices see each other on `hello` and either can send `connect_request`. If both do (routine the moment both come online), you get two independent Noise establishments — e.g. A wins LAN/TCP while B wins relay/WS simultaneously. Nothing resolves the double-win.
- **Evidence**: §5.4 line 163 (only mention of tie-break in either doc); §5.3 "both sides then race candidates"; phase-2 §2.5 "one loopback connection per (peer, remote module)… `register_module_connection` evicts the prior registration."
- **Impact**: Two concurrent Noise sessions to one peer violate the one-session-per-peer invariant the effect ledger and loopback topology assume. The second session's re-HELLO **evicts** the first, killing every in-flight call (deterministic GOODBYE → `OutcomeUnknown` on in-flight mutators), or the two connections flap in an eviction loop (constant tool disappear/reappear, CPU burn). **Ships a correctness bug that corrupts effect classification.**
- **Fix Direction**: Strike "as phase 2." Specify a NEW deterministic phase-3 rule: the **lexicographically-lower (or -higher, pick one) pubkey is the sole dial initiator**; the other side listens and MUST NOT originate a `connect_request`. Enforce a **single-session-per-peer invariant**: after a handshake completes, exchange a short authenticated `session_epoch`/`connection_attempt_id` over Noise; a second handshake to an already-connected peer is refused with a typed error; the loser tears down BEFORE any redundant re-HELLO can evict a live connection. Add a golden test where TCP and WS both complete simultaneously.

#### #3: Relay idle-teardown is misclassified as a peer partition → spurious GOODBYE / OutcomeUnknown; "classifies exactly like TCP" is a FALSE inheritance claim
- **Severity**: Critical (majority) / High (minority)
- **Confidence**: Unanimous (7 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, GLM 5.2, Gemini
- **Issue**: §6 asserts "keepalive cadence and 3× reap window unchanged; relay-path partitions classify exactly like TCP partitions." Phase-2 had exactly one carrier; "carrier-blind" / "classifies exactly like TCP" is a NEW claim about a NEW carrier, unsupported by phase-2 text (zero hits for `carrier|carrier-blind|relay-path|TCP partition`). §7's RelayDO idle-teardown creates a state TCP never has: "transport gone, peer alive." From the phase-2 reaper's viewpoint (§6.2: on missed keepalive it closes the peer's loopback connections → deterministic GOODBYE), a relay idle-teardown is indistinguishable from a true partition. The three claims cannot all hold: if keepalives traverse the pipe they reset the idle timer (pipe never idles → no cost savings, contradicting open-Q3); if they don't, the idle gap + cold-wake can exceed the 3× reap window → **spurious partition on a live peer**.
- **Evidence**: §6 lines 193-195; §7 lines 203-204; phase-2 §6.2 lines 213-214 (reaper is per-peer, TCP-designed, silent on WS/relay).
- **Impact**: In-flight effects that were actually delivered get marked `OutcomeUnknown`; for a `mutating` op (phase-2 §2.3 forbids auto-retry) the outcome is lost to the caller; the user sees tools constantly disappear/reappear. **Ships a correctness bug that loses mutation classification.**
- **Fix Direction**: Introduce an explicit **transport-reconnecting / "dormant, not partitioned"** state distinct from partition. On idle teardown the RelayDO sends a distinct close code (e.g. `4000 idle`); the fed-module keeps routes registered, suspends the reap timer for a bounded `reconnect_grace`, and initiates a fresh-grant reconnect. Reap only fires if reconnect fails within grace OR a true keepalive miss survives the reconnect. Constrain timers: `relay_idle_timeout > 3× keepalive + cold-wake + grant-issuance budget`, OR keep the pipe warm and bill continuous relay time. Retract "classifies exactly like TCP." Pairs with #4.

#### #4: Relay reconnect mid-effect / recovery reconciliation is restart-triggered only — re-opens the closed at-most-once window; "Noise resumption = cheap" is a FALSE inheritance claim
- **Severity**: Critical (minority) / Medium (minority)
- **Confidence**: Majority (5 members)
- **Members Reported**: Opus 4.8, GPT 5.4, XAI Composer, Minimax M3, Gemini
- **Issue**: Two coupled problems. (a) Phase-2 §6.1 recovery reconciliation ("on **restart** the origin queries the serving ledger") is **restart-triggered, not transport-reconnect-triggered.** A relay idle-teardown drops the pipe WITHOUT restarting the origin fed-module, so the reconciliation query never fires; an effect stuck in `sent`/`OutcomeUnknown` whose outcome sits in the serving ledger is never settled. Worse, serving-ledger retention is gated on the origin CONFIRMING outcome-received + grace — the origin can't confirm while disconnected, so a long relay outage can expire the row (`effect_outcome_expired`) → **permanently unrecoverable mutation result.** (b) §7's "Noise session resumption = ordinary re-handshake, cheap" is unsupported (phase-2 has no resumption concept); a full IK re-handshake over a fresh WS + fresh grant + possible cold-wake is O(2 RTT + DH), *acceptable* not *cheap*, and if it happens at keepalive cadence it turns a routine transport event into a ledger-reconciliation event on every idle cycle.
- **Evidence**: §7 line 204 ("cheap"); phase-2 §6.1 lines 196-207 (restart-triggered recovery, effect_id keyed on `(origin_device_pubkey, incarnation_uuid, seq)`, retention co-defined). Note: the effect_id keying itself IS pipe-agnostic and sound across reconnects — the gap is the TRIGGER and the retention-during-outage, not the id scheme.
- **Impact**: Regression of shipped BLOCKER #3 (at-most-once outcome loss) under the specific new condition of relay idle-teardown / long relay outage.
- **Fix Direction**: Make reconciliation **transport-event-triggered**: on EVERY carrier reconnect (relay or TCP), replay the §6.1 recovery query over the new pipe for every `sent`-without-outcome effect_id before resuming traffic. A relay reconnect must NOT mint a new incarnation (keeps effect_id stable — good). Gate serving-ledger grace expiry on **elapsed reachable time**, not wall-clock, so an outage can't expire an unconfirmed row. Replace "cheap" with a measured budget. Ensure session scoping and the ledger key on device_pubkey + incarnation, never the ephemeral Noise session id.

#### #5: Signaling plane has no replay protection, no nonce/timestamp/seq, and no rate limits (phase-2 relay quotas not carried to rendezvous)
- **Severity**: High
- **Confidence**: Unanimous (7 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, GLM 5.2, Gemini
- **Issue**: §5.3 signaling ops (`connect_request`, `connect_offer`, `connect_accept`, `relay_open`, `relay_grant`, `registry_delta`) carry no nonce, timestamp, or sequence, and no per-device rate limit is specified. Phase-2's anti-replay is at the Noise *frame* level (data plane only); phase-2 §5.5 gives the *relay* explicit "per-account/per-device quotas + rate limits" — phase-3 omits the analogous rendezvous controls.
- **Evidence**: §5.3 lines 146-159; phase-2 §5.5 line 184.
- **Impact**: A stolen device token or compromised cloud can flood `connect_request` (target offer-spam, wasted dials), flood `relay_open` (forced grant churn / resource exhaustion), replay a stale `connect_offer` with poisoned candidates, or replay a "device removed" `registry_delta` to force premature unpair. One AccountDO per account (single-threaded) makes this a whole-account DoS amplifier (see #12).
- **Fix Direction**: Add monotonic per-(device, WS-session) `seq` tracked in AccountDO; reject `seq ≤ last-seen`. Add a short-window server timestamp to `relay_open`/`relay_grant`. Per-device AND per-target token-bucket rate limits (e.g. 10/min per source, 30/min per target). Sign `registry_delta`/`connect_offer` with an AccountDO account-binding key the device pins at enrollment (also closes #11's authenticity gap). Bind `pipe_token` to a single `relay_open` nonce so replays mint a duplicate-rejected grant.

#### #6: Device enrollment lacks proof-of-possession and uniqueness/atomicity; enrollment races unspecified
- **Severity**: Critical (minority) / High (majority)
- **Confidence**: Majority (6 members)
- **Members Reported**: GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2, Gemini, (Minimax via #7 racing) 
- **Issue**: `POST /v1/device/enroll` accepts provider JWT + `{device_pubkey, device_name, platform}` and states "enrollment binds pubkey only" — **no signed challenge proving possession of the device private key.** Phase-2 §5.3 explicitly requires a device-held secret at enrollment "so the cloud cannot self-enroll a device" — phase-3 drops this. Also unspecified: atomic `(provider, subject) → account_id` resolution (stateless Workers can split one subject into two accounts under concurrent first-login), and unique `(account_id, device_pubkey)` (duplicate/concurrent enrollment of one pubkey → two tokens, ambiguous identity, or a pubkey-only attacker DoSing the real device by re-enrolling + rotating its token).
- **Evidence**: §4.2 lines 82-93; phase-2 §5.3 (device-held secret required). **Inheritance: NOT supported — phase-2 requires the very property phase-3 removes.**
- **Impact**: A stolen WorkOS JWT or compromised Worker enrolls an arbitrary pubkey; account split-brain partitions the device graph; pubkey-only shadow-enrollment DoSes a real device.
- **Fix Direction**: Make enroll a **challenge-response**: Worker/AccountDO issues a nonce over account/provider/device metadata; device signs with its static private key; verify against `device_pubkey` before completing. Serialize account creation through a DO keyed by `hash(provider_subject)` (or a D1 unique index). Enforce unique `(account_id, device_pubkey)` with atomic upsert-or-reject; a second enroll of an enrolled pubkey is rejected unless it carries a signed rotation proof (old key signs new enrollment — same as phase-2 rotation ceremony).

#### #7: Revocation / device removal is a best-effort delta, not a durable signed tombstone; re-enrollment of a removed pubkey re-opens a TOFU-pinning hole
- **Severity**: Critical (minority) / High (majority)
- **Confidence**: Majority (6 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2, Gemini
- **Issue**: §4.2 removal = delete record + revoke token + broadcast `registry_delta`; peers flip to `revoked_by_account`. But (a) a previously *verified* device is trusted by **pinned key, not account membership** — an established direct-TCP/relay Noise session and static-profile reachability do not use the device token, so a lost/stolen-but-verified device keeps routing until every peer applies an unordered, unsigned, droppable delta; (b) `revoked_by_account` is a NEW state absent from phase-2 (which has only `verified:true|false` + TOFU pins) and its interaction with the pin is unspecified — §5.5 says a re-appearing pubkey materializes as an ordinary `verified:false` peer, with no distinction between "never seen" and "explicitly revoked." An attacker who holds the removed device's private key re-enrolls the same pubkey; its verify-code *matches* (same key); the user re-verifying after a "device rejoined" prompt silently re-trusts the attacker. Phase-2 §6.2/§6.4 solved exactly this class with **signed revocation/tombstones + per-call generation checks** — phase-3 does not wire them in.
- **Evidence**: §4.2 lines 95-98, §5.5 lines 176-178; phase-2 §6.2 line 214, §6.4, §5.3 lines 171-174 (rotation must be old-key-signed or re-verified). **Inheritance: phase-2 supports the stronger property; phase-3 omits it.**
- **Impact**: A dropped/suppressed/replayed delta keeps a revoked device routable; same-pubkey re-enrollment defeats the "requires re-pair" promise.
- **Fix Direction**: Model removal as a **durable, locally-persisted, signed revocation tombstone** keyed by `device_pubkey` (+ instance/enrollment id) with monotonic generation. On receipt: close live Noise sessions to that pubkey immediately, invalidate outstanding relay grants, reject future handshakes/grants, and honor the tombstone over static `addr`. A tombstoned pubkey re-appearing is treated as **first contact (non-routable, full code-compare)** — never the quiet path — regardless of prior verification; re-enroll requires an explicit "previously-removed device re-enrolling — confirm" flow. Verification state must live in a **local, device-controlled store the cloud can never author** (see #8).

#### #8: Cloud-authored profile can mark a rogue peer verified — bypasses the ceremony (verification state authority)
- **Severity**: Critical
- **Confidence**: Solo (1 member) — but structurally decisive if true
- **Members Reported**: Gemini
- **Issue**: Phase-2 §3.3 says "cloud-login distributes one (signed) [profile]… The profile is the single source of truth the federation module reads," and the profile carries the peer list + egress policy. If verification state (`peer_pubkey → verified`) is part of that cloud-distributed profile, a compromised cloud distributes a tampered, cloud-signed profile marking a rogue device `verified:true` with an active allowlist — the local module trusts it and routes, **bypassing the out-of-band verify-code ceremony entirely.** This is the one candidate cross-account routability escalation in the whole review.
- **Evidence**: phase-2 §3.3, §4.3, §5.3. NOTE: phase-3 §5.5 actually says discovered peers materialize as `verified:false` and "the existing enforced gate makes them non-routable until the ceremony; discovery adds candidates, not trust" — which, if `verified` is *only ever* written locally post-ceremony, defeats this attack. The finding is that the doc must make that authority boundary **explicit and enforced**, not merely implied, and reconcile it with phase-2 §3.3's "single source of truth" phrasing.
- **Impact**: If verification is cloud-authorable, the entire discovery-only trust story collapses. High-value to nail down even though only one member surfaced it.
- **Fix Direction**: State normatively that the `peer_pubkey → verified` mapping is written **only locally, by the device, after the ceremony**, into a local-only tamper-resistant store (credentials vault / local SQLite) that the cloud never overwrites. Cloud-distributed profiles may propose *candidate* peers (`verified:false` only). The fed-module MUST ignore any cloud-supplied `verified:true`. Reconcile phase-2 §3.3's "single source of truth" wording so it excludes verification state.

#### #9: Self-reported candidates enable SSRF / internal port-scan and false-routing via a sibling or the cloud (open-Q2 answer is wrong)
- **Severity**: High
- **Confidence**: Unanimous (7 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, GLM 5.2, Gemini
- **Issue**: §5.2 LAN candidates are self-reported. Open-Q2's answer ("can only redirect its own inbound dials; Noise pins identity regardless") is **wrong**: the *dialer* is the victim. A malicious sibling or a cloud-injected `connect_offer` can advertise a LAN candidate pointing at an arbitrary internal `ip:port` on the dialer's network (`127.0.0.1`, `169.254.169.254`, RFC1918 services). The dialer performs a TCP connect + handshake attempt before Noise fails on key mismatch → **confused-deputy SSRF / port-scanner** against the victim's LAN, plus a ~2s-per-candidate DoS that funnels traffic to relay. Noise stops MITM, not the connect itself. (Opus/GPT-5.4 note a sharper variant: if the attacker separately holds the peer's static key, the code *matches* and the operator has no signal the dial went to attacker infra.)
- **Evidence**: §5.2 lines 126-143, §5.4 lines 161-169, open-Q2 lines 255-257.
- **Impact**: Internal-network SSRF/scan, dial-timeout DoS, relay-funnel metadata exposure.
- **Fix Direction**: Validate candidates before dialing: reject loopback/link-local/multicast/metadata ranges; require LAN candidates to fall within the dialer's own observed private subnet; the Worker MUST always overwrite (never accept) the device-supplied `public` value; cap dial fan-out; treat Noise-handshake failure as **immediate-fail** (don't burn the full 2s). For unverified peers, prefer public+relay only; enable self-reported LAN dialing only after a same-network plausibility signal (or a local mDNS/LLMNR confirmation of the pubkey fingerprint).

#### #10: Device token is a pure bearer with no proof-of-possession; revocation/rotation races on live control WS
- **Severity**: High
- **Confidence**: Majority (6 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Gemini, (Minimax token custody)
- **Issue**: §4.2 token is an "opaque bearer" — possession = full signaling identity. File theft alone (no private key) lets an attacker rewrite the victim's own candidates (redirect its inbound dials), flood signaling, consume its slot. Separately, revocation/rotation semantics for *already-open* sessions are unspecified: an attacker with a stolen token can keep an authenticated control WS and keep signaling after nominal revocation; token rotation has a "two valid tokens" window an attacker can race.
- **Evidence**: §4.2 lines 86-99, §5.1, threat table §8 line 222 (treats "cannot speak Noise" as sufficient, ignoring signaling-plane takeover).
- **Impact**: Signaling-plane impersonation over the theft/rotation window: candidate poisoning, `relay_open` grinds, DoS. No routability (Noise still gates it) — containable but real.
- **Fix Direction**: Make it a **proof-of-possession** token — the `hello`/WS-auth challenge is signed by the device Noise static key (token names the device, signature proves custody); file theft without the private key then yields nothing. Give tokens `token_id`/version/expiry; on revoke/rotate, atomically invalidate the old, close all that device's sessions immediately (no two-valid-token window), invalidate unconsumed grants derived from the old version; RelayDO checks token validity per-frame, not just at upgrade.

#### #11: WS carrier framing — length-prefix / message-boundary mismatch behavior is unspecified
- **Severity**: Medium
- **Confidence**: Unanimous (7 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, GLM 5.2, Gemini
- **Issue**: §6 keeps the 4-byte length prefix "redundant but identical" inside one-record-per-WS-message framing but never states receiver behavior on: a message with ≠1 record (zero or concatenated), or a prefix that disagrees with the WS payload length. A stream parser fed a length-inconsistent message either blocks awaiting bytes that never arrive, buffers across messages (re-introducing the ambiguity the framing was meant to remove), or silently truncates — a desync / DoS surface (Noise AEAD contains injection, but not the framing DoS).
- **Evidence**: §6 lines 187-191.
- **Impact**: Carrier desync, hung parser, session-teardown DoS on malformed/malicious input.
- **Fix Direction**: Normative receiver invariant: assert `ws_message_len == 4 + prefix_value` and exactly one record per message; on any mismatch, zero-record, or multi-record message, **close the carrier with a typed framing error** — never silently truncate or cross-message-buffer. The prefix is a consistency check, not a length source. Add golden negative-vectors for each mismatch case.

#### #12: AccountDO-per-account is a single-threaded chokepoint / DoS-amplification point
- **Severity**: Medium
- **Confidence**: Unanimous (7 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, GLM 5.2, Gemini
- **Issue**: One single-threaded DO per account holds all live WS + registry + signaling. One flooding/misbehaving device (see #5) degrades every device on the account; geographically dispersed devices pay cross-region latency to the DO home.
- **Evidence**: §5.1 line 119.
- **Impact**: Whole-account signaling starvation under flood; latency tax. Fine for a few-device personal account; must be bounded before paid-tier/team scale.
- **Fix Direction**: Document per-account throughput ceiling + failure mode (backpressure/queue depth). Enforce per-device fairness/rate limits (#5). Cap devices per account for v1 (e.g. 50). Defer a signaling-only sub-DO shard / registry-in-D1 split to phase 4+.

#### #13: WS hibernation cold-wake can blow the ~2s candidate timeout (open-Q5)
- **Severity**: Medium
- **Confidence**: Majority (6 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, Gemini
- **Issue**: §5.1 "hibernation keeps idle accounts at ~zero cost" vs §5.4 "short per-candidate timeout (~2s)." A DO cold-wake (tens of ms best case, >1s worst) on the target account's first signal can arrive after the initiator's per-candidate timeout has already fired → unnecessary relay fallback or dial failure. The 2s number is unjustified against cold-wake.
- **Evidence**: §5.1 line 121, §5.4 line 164, open-Q5.
- **Impact**: Spurious relay fallback / dial failure on first-contact to an idle account.
- **Fix Direction**: **Decouple the signaling deadline from the candidate-dial timeout.** Give `connect_request → connect_accept` its own budget (≈5s, absorbing cold-wake p99); the per-candidate ~2s timer starts only AFTER `connect_accept`. Warm the target DO on the initiator's `connect_request`; keep the WS warm during active use, hibernate after N minutes idle. Measure cold-wake p99 in the 3a-1 Miniflare/workerd tests and set the deadline from data.

#### #14: Rendezvous present-but-down fallback to static profiles is asserted but not engineered
- **Severity**: High (majority) / Medium (minority)
- **Confidence**: Majority (6 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Minimax M3, Gemini
- **Issue**: §5.5 guarantees `[rendezvous]`-*absent* = exact phase-2 behavior (clean). But *present-but-down* is unspecified: if dialing is gated on receipt of a `connect_offer` and the control WS is down/hung, a peer with a perfectly good static `addr` candidate becomes unreachable — the exact failure the fallback exists to prevent. The doc never states static candidates are dialed on a path independent of the signaling round-trip, never bounds the control-WS connect timeout, and never says how the module distinguishes "registry says zero candidates" from "registry unreachable." (Gemini adds: discovered candidates may not be persisted locally, so a same-LAN discovered peer is lost across a restart during a cloud outage.)
- **Evidence**: §5.4, §5.5 lines 173-180, §8 lines 225-226.
- **Impact**: A cloud outage bricks an otherwise-static-reachable peer / hangs the UX — violates the "airtight degradation" requirement.
- **Fix Direction**: Specify a **signaling-independent direct-dial path**: any peer with a static/last-known candidate is dialed immediately on startup regardless of control-WS state; bounded control-WS connect timeout (2–5s), then proceed on static candidates and retry rendezvous in the background. Add a `rendezvous` status field so the module distinguishes down-vs-empty. Persist discovered candidates locally. Add an acceptance test: kill the Worker mid-run → static-reachable peer stays connected; discovered peer fails cleanly with `rendezvous_unreachable` (no hang).

#### #15: Registry candidate staleness/versioning — stale/flapping candidates cause dead/wrong dials; no offer-freshness binding
- **Severity**: Medium
- **Confidence**: Majority (4 members)
- **Members Reported**: GPT 5.4, GPT 5.5, Gemini, (Opus via #F-note)
- **Issue**: Candidates are "self-reported, refreshed on connect + change" with no candidate generation, expiry, or offer-freshness binding. A device whose LAN IP changed (DHCP) but hasn't pushed a delta leaves stale candidates in peers' caches; a `connect_accept` can carry candidates differing from the snapshot used at offer time (a downgrade/redirect vector via a sibling token). Online flaps mid-dial race the ~2s loop.
- **Evidence**: §5.2 lines 131-138, §5.3 `connect_accept {candidates}` (no freshness tag), §5.4.
- **Impact**: Wasted dial timeouts, unnecessary relay fallback, a downgrade vector.
- **Fix Direction**: Add server-issued `candidate_generation`/`observed_at`/`expires_at` and an `offer_nonce` echoed in offer/accept. Ignore expired/stale offers; require `connect_accept` candidates to be a subset of the answerer's current registry row (DO stamps a snapshot hash); coalesce flapping updates; treat Noise-handshake failure as immediate-fail and advance.

#### #16: Device clock assumptions — TTLs / last_seen / token expiry must be server-authoritative
- **Severity**: Low (majority) / Medium (minority)
- **Confidence**: Majority (5 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI Composer, Gemini, Minimax
- **Issue**: The doc doesn't state that security-relevant TTLs (token expiry, pipe_token TTL, candidate freshness) and `last_seen_ms` are evaluated against **server** time. A skewed/rolled-back device clock could self-discard a valid token (self-DoS) or self-report a future `last_seen_ms` (stays "online"); a rolled-back monotonic-vs-wallclock reaper could extend a partitioned peer's apparent liveness.
- **Evidence**: §5.2 `last_seen_ms` (provenance unclear — line 142 only stamps `public` server-side), §7 "short TTL", §4.2 token rotation.
- **Impact**: Self-DoS, false online bit, reaper timing bug on clock rollback. Mostly benign; one real correctness edge (reaper).
- **Fix Direction**: State normatively: ALL security TTLs and `last_seen_ms` are server-stamped/enforced (Cloudflare wall-clock); devices never make trust/TTL decisions on local wall-clock; grants/offers carry server-issued expiry; the reaper uses the device **monotonic** clock (immune to wall-clock jumps), not wall time.

#### #17: "Loud device-join events" are a paper defense unless authenticated and un-dismissible
- **Severity**: Medium (minority) / Low (minority)
- **Confidence**: Minority (3 members)
- **Members Reported**: GPT 5.5, Minimax M3, Opus (via F13 confusion attack)
- **Issue**: §4.3/§8 lean on "device-join events pushed loudly" as the control that makes rogue-device enrollment detectable, but the mechanism is only asserted. If the Worker can push a fake "join" event, the loud UX is itself a phishing surface; if the event is dismissible/missable or delivered as an Accept/Deny prompt rather than a code-compare, the defense fails. Opus's variant: a cloud that controls which join events surface can drive a code-comparison *confusion* attack (user compares against the wrong screen).
- **Evidence**: §4.3 line 110, §8 line 220.
- **Impact**: The detectability control the threat model relies on may not hold in practice.
- **Fix Direction**: `registry_delta {device_added,…}` signed by the AccountDO (device pins the account key at enrollment — ties to #5); offline devices get the backlog on next `hello` (retained N days); CK app/fed-cli renders an **un-dismissible** banner showing the new device's verify-code until explicitly acknowledged; the ceremony UX for any unverified offer is **hard-locked to code-compare** ("Codes match" is the only accept gesture — no Accept/Deny), bound to a user-initiated pairing action, never to a cloud-pushed event.

---

## Summary Table

| # | Finding | Severity | Agreement | Members |
|---|---------|----------|-----------|---------|
| 1 | Pipe-token contradiction / redemption authz | Critical | Unanimous | 7 |
| 2 | Candidate racing: false tie-break inheritance + double-win | Critical | Unanimous | 7 |
| 3 | Relay idle-teardown → false partition; "= TCP" false | Critical | Unanimous | 7 |
| 4 | Relay reconnect vs restart-only reconciliation; "cheap" false | Critical/Med | Majority | 5 |
| 5 | Signaling replay / no rate limits (relay quotas not inherited) | High | Unanimous | 7 |
| 6 | Enrollment: no PoP, no uniqueness/atomicity, races | Critical/High | Majority | 6 |
| 7 | Revocation not a durable signed tombstone; re-enroll hole | Critical/High | Majority | 6 |
| 8 | Cloud-authored profile can mark rogue peer verified | Critical | Solo | 1 |
| 9 | Candidate SSRF / port-scan / false-routing (Q2 answer wrong) | High | Unanimous | 7 |
| 10 | Device token bearer (no PoP); revoke/rotate races | High | Majority | 6 |
| 11 | WS framing length/boundary mismatch unspecified | Medium | Unanimous | 7 |
| 12 | AccountDO single-threaded hot spot / DoS amplifier | Medium | Unanimous | 7 |
| 13 | WS hibernation cold-wake blows 2s candidate timeout | Medium | Majority | 6 |
| 14 | Rendezvous present-but-down fallback not engineered | High/Med | Majority | 6 |
| 15 | Registry candidate staleness / no offer-freshness binding | Medium | Majority | 4 |
| 16 | Device clock: TTL/last_seen must be server-authoritative | Low/Med | Majority | 5 |
| 17 | "Loud join" paper defense unless authenticated/un-dismissible | Medium/Low | Minority | 3 |

---

## Answers to the Five Open Questions (council consensus)

**Q1 — Device-token custody on disk.** *Consensus: 0600-plaintext-bearer is not sufficient; harden.* Split view on the exact remedy, converging on: the real fix is not the vault per se but removing bearer semantics — make the token **proof-of-possession** (signed by the device static key, #10), after which co-locating it at 0600 next to the (equally sensitive) private key is fine. If bearer semantics are kept for v1, then **encrypt at rest keyed by a per-host secret derived from the device static key** (HKDF), so a stolen file is useless on another host, and/or fold into the credentials vault. Two members (GLM, Minimax) accept plain 0600 for v1 on the "same bar as the private key" argument — a defensible minority, but the majority note the token is the *rendezvous* attack surface and deserves stronger custody. **Recommendation: PoP token (#10) + 0600 co-location; vault integration as a fast follow, not a v1 gate.**

**Q2 — Registry candidate self-reporting.** *Consensus (unanimous): the doc's current answer is wrong; harden before build.* Noise pins identity but the *dialer* is the SSRF/port-scan victim (#9). Required: Worker always overwrites (never trusts) device-supplied `public`; dialer rejects loopback/link-local/multicast/metadata; LAN candidates restricted to the dialer's own observed subnet (or omitted for unverified peers); Noise-handshake failure is immediate-fail, not full-timeout; rate-limit candidate churn; WS writes may only mutate the caller's own row.

**Q3 — Relay pipe lifetime.** *Consensus: per-connection grants are acceptable ONLY after #3/#4 are fixed, with idle-timeout raised well above keepalive cadence.* Reconnect-per-*keepalive* churn is unanimously **not** acceptable. Preferred shape: a **standing/long-lived pipe per active peer-pair** kept warm while a session is live (metered — it's the paid-tier meter anyway), idle-teardown only after the fed session itself is idle beyond the reap envelope, with a "dormant, not partitioned" state so idle-close never evicts the catalog or fires GOODBYE. Set `idle_timeout ≥ max(10× keepalive, 5 min)`. Measure reconnect frequency in the 3b-2 drill.

**Q4 — connect_offer to unverified.** *Consensus: offer-to-unverified is required for the pairing UX (chicken-and-egg: you need a channel to compare codes), so keep it ALLOWED — but not "loud and ambient."* Add: the target must OPT IN / user-initiate a short-lived pairing window (mature accounts can go quiet via `accept_unverified_offers:false`); strict per-initiator rate limits; and the target UX **hard-locked to code-compare** (single "Codes match" gesture, never Accept/Deny), bound to a local pairing action, never a cloud-pushed event. Opus/GPT-5.4/GPT-5.5 lean toward "verified-only by default + explicit exception window"; the rest toward "allowed + rate-limited + opt-out" — functionally the same guardrails.

**Q5 — WS hibernation cold-wake.** *Consensus: acceptable on first signal IF the signaling deadline is decoupled from the ~2s candidate-dial timeout (#13).* Give signaling its own ≥5s budget; warm the DO on `connect_request`; keep WS warm during active use; measure cold-wake p99 in 3a-1 and tune from data. The unanimous red line: cold-wake must never count against the per-candidate dial budget.

---

## Overall Verdict: **GO-WITH-CHANGES** (equivalently: NO-GO until the must-fix blockers are specified)

The seven verdicts (3 GO-WITH-CHANGES, 3 NO-GO, 1 hybrid) are unanimous in substance: **the architecture is sound and the phase-0-2 trust core is correctly inherited and survives adversarial scrutiny** — no member found a routability/plaintext escalation that defeats the Noise-IK + verify-code ceremony (the sole candidate, #8, is a documentation-authority ambiguity to nail down, not a demonstrated break). The gaps are in the *implementation of the locked decisions* and in a recurring anti-pattern of **presenting new phase-3 mechanisms as inherited phase-2 properties**, which must be corrected so the council gate scrutinizes them as new.

**Three inheritance claims are FALSE against phase-2 v4.1 (must be re-framed as new, explicitly-specified mechanisms):**
1. "pubkey tie-break as phase 2" (#2) — no tie-break exists in phase-2.
2. "relay-path partitions classify exactly like TCP" / "carrier-blind" (#3) — phase-2 had one carrier; the reaper is TCP-designed and per-peer.
3. "Noise session resumption = ordinary re-handshake, cheap" (#4) — phase-2 has no resumption concept; recovery is restart-triggered, not reconnect-triggered.

**Must-fix-before-build (blocker set — union of what members gated on):**
- **#1** Pipe-token: per-side, pubkey-bound, atomic single-use, no cross-device redemption; fix the §5.3/§7 schema contradiction.
- **#2** Deterministic single-initiator tie-break + single-session-per-peer invariant (fixes double-win / eviction flap).
- **#3** Explicit transport-reconnecting/"dormant" state so relay idle-teardown ≠ partition; retract "= TCP"; constrain timers.
- **#4** Transport-reconnect-triggered recovery reconciliation + outage-safe (elapsed-reachable-time) retention; drop "cheap."
- **#6** Enrollment proof-of-possession + atomic uniqueness on `(provider,subject)→account_id` and `(account_id,device_pubkey)`.
- **#7** Durable signed revocation tombstone; re-enrolled removed pubkey = first-contact, never quiet re-trust.
- **#8** Verification state is local-authored only; the cloud can never set `verified:true`.

**Strongly recommended before the gate (HIGH):** #5 (signaling replay + rate limits + signed deltas), #9 (candidate SSRF filtering), #10 (PoP device token + revoke/rotate session teardown), #14 (rendezvous-down static fallback engineered + tested).

**Schedulable within the phase (MEDIUM/LOW):** #11 (WS framing invariant + negative vectors), #12 (DO ceiling + fairness), #13 (decoupled signaling deadline), #15 (candidate freshness), #16 (server-authoritative clocks), #17 (authenticated un-dismissible join events).

**Confidence: HIGH.** The three false-inheritance findings and the pipe-token contradiction are direct textual facts checkable against both docs; the blocker set is convergent across independent models with near-identical fixes. No wall-of-blockers inflation — severity discrimination is honest (the MEDIUM/LOW tail is genuinely lower-stakes).
