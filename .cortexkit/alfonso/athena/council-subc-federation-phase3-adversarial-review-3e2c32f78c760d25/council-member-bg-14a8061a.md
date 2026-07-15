## Finding 1: Pubkey tie-break “inherited from phase 2” is unsupported
- **Severity**: HIGH
- **Location**: 5.4 (phase-3); inheritance claim vs `docs/subc-federation-design.md`
- **Confidence**: high
- **Issue**: Phase-3 says the initiator for candidate racing is chosen by “pubkey tie-break as phase 2.” Phase-2 v4.1 contains **no** pubkey tie-break, simultaneous `connect_request`, or single-initiator dial policy anywhere (only phase-3 5.4 mentions it). Simultaneous `connect_request` from both devices is plausible once both see registry entries; without a normative rule you get duplicate dials, conflicting initiator candidate orders, or two live transports.
- **Evidence**: Grep across repo: tie-break appears only in `docs/subc-federation-phase3-design.md:163`. Phase-2 describes session establishment only as “federation module establishes Noise IK sessions” (3.1, 4.1) with no collision semantics.
- **Suggested Fix**: Specify in phase-3 (and optionally backport to phase-2 appendix): e.g. lower static pubkey hex is sole dial initiator for `connect_request`; the other side must not originate dial until `connect_accept` or must ignore duplicate offers when already answering; define teardown if both TCP and WS Noise sessions complete (prefer lower-pubkey side’s winning carrier; close the loser within one RTT).

## Finding 2: Dual-carrier win (TCP + WS) not resolved
- **Severity**: BLOCKER
- **Location**: 5.4, 
- **Confidence**: high
- **Issue**: “First-success-wins” is per candidate list on the initiator, but **both peers** run racing after `connect_offer`/`connect_accept`. Nothing prevents peer A winning on `lan` (TCP) while peer B wins on `relay` (WS) at the same time → two Noise sessions, duplicate loopback bridges per (peer, module), split effect streams, and ledger/dedup keyed on transport-agnostic effects but **two ingress paths** for the same peer pubkey.
- **Evidence**: 5.3: “both sides then race candidates”; 5.4: first-success-wins with no cross-carrier dedup or “single active session” invariant.
- **Suggested Fix**: One normative session per peer pubkey: (1) tie-break picks exactly one dialer; answerer only accepts inbound from dialer’s chosen path until session established; (2) after handshake, exchange a short authenticated `session_epoch` over Noise and drop any second handshake with mismatched epoch; (3) fed-module closes redundant carrier immediately on duplicate IK completion.

## Finding 3: `pipe_token` grant semantics allow cross-device redemption (as written)
- **Severity**: BLOCKER
- **Location**: 5.3 `relay_grant`,  RelayDO
- **Confidence**: medium-high
- **Issue**: Grant shows one `pipe_token` “issued to both sides” with “per-side `pipe_token`” in  — contradictory. If both sides share one token, any holder of the token (stolen device token + intercepted grant, or malicious sibling on same account WS) can occupy a slot on the pipe before the legitimate peer, causing DoS or wrong-bridge confusion. If tokens are per-side, the doc must say how RelayDO binds `pipe_token` → `(pipe_id, expected_device_pubkey)` and rejects the wrong side’s token on the wrong socket.
- **Evidence**: 5.3: `{relay_url, pipe_id, pipe_token}` singular;  “per-side `pipe_token`, single-use, short TTL” without binding rules or consumption site (Worker vs RelayDO).
- **Suggested Fix**: Mint **two** tokens: `pipe_token_a` bound to pubkey A, `pipe_token_b` to pubkey B; RelayDO stores expected pubkeys from AccountDO-signed grant; first WS auth presents token+pubkey fingerprint; single-use enforced in RelayDO SQLite with atomic consume; TTL enforced on server clock; reject third connection.

## Finding 4: Signaling has no replay, ordering, or flood controls
- **Severity**: HIGH
- **Location**: 5.3,  threat table
- **Confidence**: high
- **Issue**: Control messages are JSON with no `msg_id`, timestamp, or HMAC tied to device token. A compromised sibling (valid device token) or replayed WS payload can flood `connect_request` / `relay_open` to all pubkeys on the account. AccountDO is one DO per account — no per-device rate limits documented. Phase-2 5.5 requires relay “auth-before-resource + quotas”; phase-3 rendezvous omits the analogous controls.
- **Evidence**: 5.3 ops list only;  mentions stolen device token for signaling but not rate limits; phase-2 5.5 vs absent in phase-3 .
- **Suggested Fix**: AccountDO: per-device token bucket for `connect_request`/`relay_open`; monotonic `seq` per device on outbound signals with DO rejecting `seq <= last_seen`; optional short-lived HMAC on signaling bodies keyed from hashed device token; push `registry_delta` with same seq discipline.

## Finding 5: Device-token revocation vs in-flight control WS and signaling
- **Severity**: HIGH
- **Location**: 4.2, 5.1, 
- **Confidence**: high
- **Issue**: Removal revokes token and broadcasts delta, but no rule for **already-open** control WebSockets authenticated with the old token, or in-flight `connect_request` issued milliseconds before revoke. Attacker with stolen token could keep signaling until WS is force-closed; legitimate revoke might not kill active sessions promptly.
- **Evidence**: 4.2 “revokes the token”; 5.1 “device-token check on everything else” — no explicit WS disconnect on revoke, no grace period semantics.
- **Suggested Fix**: On revoke/delete: AccountDO closes that device’s control WS with defined close code; rejects all messages with revoked token hash immediately; invalidates pending `connect_offer`/`relay_grant` initiated by that device; peers treat `registry_delta` remove as hard unpair (already partially in 4.2) and tear down data sessions to that pubkey.

## Finding 6: Enrollment pubkey collision and re-enrollment after removal underspecified
- **Severity**: HIGH
- **Location**: 4.2, 4.2 device removal
- **Confidence**: high
- **Issue**: Two enrollments with the same `device_pubkey` (clone key, restore from backup, malicious second device): which wins, is the old token invalidated, do peers auto-update or treat as key rotation? Re-enrollment after `revoked_by_account` requires re-pair, but if cloud re-admits same pubkey without clearing local `revoked_by_account` + pinned key state on peers, you get permanent non-routability or stale TOFU. Phase-2 rotation requires old-key signature or full re-verification (5.3); phase-3 removal path doesn’t wire to that ceremony.
- **Evidence**: 4.2 steps 1–4 bind pubkey only once; no uniqueness constraint described; 4.2 removal flips `revoked_by_account` locally but no cloud→peer sync of “same pubkey new enrollment epoch.”
- **Suggested Fix**: AccountDO: unique index on `device_pubkey` per account; re-enroll same pubkey only after explicit removal + new enrollment **epoch** in registry; push delta includes `enrollment_id` ULID; fed-module treats pubkey+enrollment_id change as first-contact (non-routable, verify-code), not silent update.

## Finding 7: Candidate poisoning by sibling device (LAN) — partial DoS, inheritance of Noise pin understated
- **Severity**: MEDIUM
- **Location**: 5.2,  Q2, 
- **Confidence**: high
- **Issue**: Each device self-reports `lan` candidates. A malicious sibling cannot impersonate Noise as another pubkey without the private key, but it **can** publish garbage `lan`/`public` listen ports on **its own** registry row to waste dialer timeouts (~2s each) and push victims toward relay. If DO allows updating another device’s row (bug/missing binding), that becomes redirect — doc assumes honest self-report only.
- **Evidence**: 5.2 “self-reported”;  Q2 answer in doc: “only redirect its own inbound dials” — true for identity, false for **availability** (dialer wastes 2s × candidates on attacker-chosen ordering if combined with signaling spam).
- **Suggested Fix**: Server never trusts sibling-reported `public` (already server-stamped); for `lan`, optionally omit from snapshot unless requester shares RFC1918 overlap (heuristic) or LAN candidate is learned only via local mDNS later; rate-limit candidate churn; enforce WS writes only mutate caller’s pubkey row.

## Finding 8: `connect_offer` to unverified peers — metadata harassment and “loud” vs routability window
- **Severity**: MEDIUM
- **Location**: 4.3, 5.3, 5.5,  Q4; phase-2 5.3
- **Confidence**: high
- **Issue**: Verified-gate blocks **tool routability** (phase-2 5.3 non-routable until ceremony; phase-3 5.5 `verified:false`). Any account member (or stolen device token) can still `connect_request` unverified targets, causing offers, dial races, and relay grant churn — harassment and cost, not cross-account routability. Claim that verify-code makes cloud compromise unable to achieve routability holds for **mutations** if ceremony is enforced; it does **not** make discovered peers “non-routable” for transport setup attempts.
- **Evidence**: 5.3 full signaling to registry pubkeys; 4.3 “rogue … offer for pairing”; phase-2 requires ceremony before exposure allow-list, not before Noise transport for pairing UX.
- **Suggested Fix**: Policy choice (see open Q4): either allow offers but cap rate and require user acknowledgment on first offer from unknown-fingerprint; or gate `connect_request` relay at AccountDO until requester has local `verified` flag synced via optional `verification_state` bit (still not a substitute for cryptographic ceremony on exposure).

## Finding 9: Relay idle teardown vs keepalive/reaper — false partition risk
- **Severity**: HIGH
- **Location**:   inheritance from phase-2 6.2
- **Confidence**: medium
- **Issue**: Phase-3 asserts “relay-path partitions classify exactly like TCP partitions” with unchanged keepalive/reap. RelayDO **idle timeout tears pipe down** while TCP might stay up longer. Fed-module may see sudden WS close → reaper marks partition → closes loopback → `OutcomeUnknown` for in-flight mutators; then reconnect + re-handshake. If idle timeout < 3× keepalive window or keepalive doesn’t traverse relay path identically, you get **spurious partitions** and recovery churn. Phase-2 reaper is authoritative on missed keepalive (6.2); coupling relay idle policy to that window is unstated.
- **Evidence**:  idle timeout;  unchanged cadence; phase-2 6.2 partition via reaper, not transport-specific caveats.
- **Suggested Fix**: Relay idle timeout ≥ 3× keepalive interval + margin; keepalive frames must be forwarded by RelayDO verbatim; on relay teardown classify as transport reset but **debounce** partition declaration (e.g. one failed re-handshake within T before reaper fires); document interaction with 6.1 recovery (query serving ledger before declaring `not_sent`).

## Finding 10: WsCarrier framing mismatch behavior unspecified
- **Severity**: HIGH
- **Location**:  WsCarrier
- **Confidence**: high
- **Issue**: One WS binary message must carry exactly one fed record with embedded 4-byte length prefix. Adversary or bug can send: empty message, multiple records in one message, length prefix disagreeing with `len(ws_payload)`, or prefix larger than WS frame. Receiver behavior (close session? single GOODBYE?) affects partition and effect classification identically to malicious drop.
- **Evidence**:  describes redundancy of prefix but no validation rules or failure mode.
- **Suggested Fix**: Normative parser: `ws_payload.len() >= 4`, `len == u32_be(prefix)`; exactly one record per message; on mismatch close carrier and signal transport fault without corrupting ledger; golden tests for mismatch vectors.

## Finding 11: Rendezvous present-but-down fallback not airtight
- **Severity**: HIGH
- **Location**: 5.5 `[rendezvous]` absent vs present;  “static profiles remain fallback”
- **Confidence**: medium
- **Issue**: Absent `[rendezvous]` → phase-2 static behavior is clear. If `[rendezvous]` is present and control_url unreachable, doc doesn’t define: blocking on first tool call vs background reconnect, whether static `addr` candidates still dial in parallel, or timeout before abandoning cloud path. Risk: hung discovery-only peers with no static addr → total loss of connectivity despite phase-2-capable static profile hybrid.
- **Evidence**: 5.5 hybrid static addr “highest priority” but no failure policy when control plane is down.
- **Suggested Fix**: Fed-module dials static/highest-priority candidates without waiting for `registry_snapshot`; control WS reconnect with exponential backoff; if snapshot not received within T, operate on last-known registry + static peers only; never block TCP-only phase-2 peers on cloud login.

## Finding 12: Registry staleness mid-dial
- **Severity**: MEDIUM
- **Location**: 5.2, 5.4
- **Confidence**: high
- **Issue**: `registry_delta` can change candidates while initiator is in ~2s per-candidate loop. Device may go offline, `public` candidate may change on reconnect, or token revoked. Dialer may complete Noise to wrong endpoint (stale public IP) or waste time then fall back — OK — but no rule for accepting `connect_accept` candidates that differ from snapshot used at offer time (downgrade attack via malicious answerer with sibling token).
- **Evidence**: 5.3 `connect_accept {candidates}` without freshness binding to offer.
- **Suggested Fix**: Include `offer_nonce` in offer/accept; accept only if answerer pubkey matches registry; candidates in accept must be subset of answerer’s current registry row as of AccountDO relay time (DO stamps snapshot hash).

## Finding 13: Phase-2 relay “auth-before-resource + quotas” not inherited for RelayDO
- **Severity**: MEDIUM
- **Location**:  vs phase-2 5.5
- **Confidence**: high
- **Issue**: Phase-2 requires relay auth-before-resource and quotas. Phase-3 RelayDO mentions tokens and frame cap but not per-account relay byte/minute quotas or allocation before pipe bridge.
- **Evidence**: phase-2 5.5; phase-3  properties list zero-knowledge and 16 MiB cap only.
- **Suggested Fix**: Worker/AccountDO issues grant only after quota check; RelayDO refuses bridge until both auths; meter WS bytes per account for DoS containment.

## Finding 14: Device clock reliance (TTL, `last_seen_ms`)
- **Severity**: LOW
- **Location**: 5.2 `last_seen_ms`,  TTL
- **Confidence**: medium
- **Issue**: If `last_seen_ms` is client-supplied, skew enables bogus online bits. TTL for pipe_token should be server-issued only (implied but not stated).
- **Suggested Fix**: `last_seen_ms` and online bit updated only by AccountDO on hello/heartbeat; ignore client timestamps for security decisions.

## Finding 15: “Noise session resumption = ordinary re-handshake, cheap” vs fed-module session state
- **Severity**: MEDIUM
- **Location**:  phase-2 6.1 recovery, 4.1 catalog on session
- **Confidence**: medium
- **Issue**: Re-handshake is fine for Noise IK, but phase-2 ties catalog sync and effect recovery to **reachable peer** and durable effect_ids, not to ephemeral session tickets. After relay reconnect mid-effect, recovery must use serving ledger — phase-2 supports that (6.1 recovery row). Risk: reaper declares partition during relay blip before recovery runs → premature GOODBYE. Inheritance partially holds; coupling is underspecified (see Finding 9).
- **Evidence**:  “cheap” re-handshake; phase-2 6.1 recovery on restart/reachable peer.
- **Suggested Fix**: Treat relay reconnect as transport flap: do not increment incarnation; retry ledger query on new session before reaper settlement; document in .

---

## ANSWERS TO THE FIVE OPEN QUESTIONS

1. **Device-token custody (0600 file)**  
   **Recommendation**: Accept 0600 file adjacent to device key for v1 **only if** same user/backup posture as device key; document that theft of key file implies theft of device token. Defer full credentials vault to phase 3.1+ **unless** CK app multi-device sync is in scope — then vault sooner. Add mandatory `chmod` check at `fed-cli login` and refuse world-readable paths.

2. **Registry candidates self-reporting**  
   **Recommendation**: Keep self-report for v1 with **server-side enforcement**: WS candidate updates apply only to authenticated pubkey; rate-limit updates; do not propagate another device’s `lan` into third-party dial lists without optional “same site” heuristic; rely on Noise IK for identity. Add explicit UX note: malicious sibling can waste ~2s per dial (mitigate with rate limits), not impersonate.

3. **Relay pipe lifetime (per-connection vs standing)**  
   **Recommendation**: Per-connection grants with idle teardown **acceptable** if idle timeout ≥ 3× keepalive and reconnect is automatic via new `relay_open` without user action; avoid standing pipes in v1 (simpler single-use tokens, smaller blast radius on token leak). Measure churn in 3b-2 gate; if keepalive exceeds idle, fix timeout math first.

4. **`connect_offer` requires verified target?**  
   **Recommendation**: **Allow offer-to-unverified** for pairing UX (registry discovery → ceremony), but gate **relay_open** and high-frequency `connect_request` behind: (a) rate limits, (b) optional “pairing mode” flag on first contact, (c) loud join already in 4.3. Do **not** require verified before relaying offers if ceremony is purely local from registry pubkeys; require verified before **auto** data-session retry policies to avoid harassment-only paths.

5. **WS hibernation vs signaling latency**  
   **Recommendation**: Accept one cold-wake RTT on first signal to idle account **if** signaling and enroll paths use DO `waitUntil`/`blockConcurrencyWhile` patterns and client timeouts for `connect_request` ≥ cold-wake p99 + 2× candidate dial budget (~5–8s minimum, not ~2s only on first hop). Document p99 wake in gate metrics; fail open to static candidates in hybrid profiles.

---

## Summary

| Severity | Count |
|----------|-------|
| BLOCKER | 2 |
| HIGH | 8 |
| MEDIUM | 5 |
| LOW | 1 |

**Overall verdict: GO-WITH-CHANGES (NO-GO until blockers fixed).**

**Must-fix before build (blockers):**
- **F2**: Single-session invariant across simultaneous candidate racing / dual carriers (TCP+WS).
- **F3**: Per-side `pipe_token` binding to pubkey, single-use consume semantics, and grant message schema fixed.

**Strongly recommended before gate (HIGH):** pubkey tie-break spec (phase-2 gap), signaling replay/flood limits, revocation WS teardown, relay idle vs reaper coupling, WsCarrier parse failures, rendezvous-down hybrid dial policy, enrollment/re-enrollment epoch + uniqueness.

**Inheritance checks:** Verified-gate / non-routable until ceremony is **supported** by phase-2 5.3 and 3.3. Pubkey tie-break and “relay partitions = TCP partitions” are **not fully supported** by phase-2 text (partition reaper yes; relay idle and tie-break no). Relay quotas/auth-before-resource from phase-2 5.5 are **not carried forward** in phase-3  as written.