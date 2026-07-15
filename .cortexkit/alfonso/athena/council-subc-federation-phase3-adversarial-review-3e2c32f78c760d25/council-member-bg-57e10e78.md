## Finding P3-B1: Device removal revokes rendezvous auth, not data-plane routability
- **Severity**: BLOCKER
- **Exact section it hits**: Phase-3 4.2, 5.5,  inherited from phase-2 6.2/6.4
- **Confidence**: High
- **The problem**: Device removal is specified as “delete record + revoke token + broadcast delta,” but established direct TCP/relay Noise sessions and static-profile reachability do not use the device token. A lost/stolen but previously verified device with its private key can keep an existing Noise session alive, or continue over static/direct paths, until every peer applies an unordered registry delta. Re-enrollment of the same pubkey after `revoked_by_account` is also only stated, not mechanized.
- **Evidence**: Phase-3 4.2 says removal revokes the token and broadcasts a delta;  says relay pipes are independent WebSockets after grant. Phase-2 supports revocation only with signed revocation/tombstones and per-call generation checks (6.2/6.4); phase-3 does not wire those into AccountDO removal, relay grants, live sessions, or static fallback.
- **Concrete fix**: Add a monotonic revocation tombstone ledger keyed by `device_pubkey` plus `device_instance_id`; persist it locally; close live Noise sessions immediately on receipt; reject future handshakes/grants for revoked keys; make static profiles honor tombstones over static `addr`; invalidate outstanding relay grants on revoke; require explicit re-pair to clear `revoked_by_account`, even for the same pubkey.

## Finding P3-H1: Enrollment lacks private-key proof and uniqueness/race semantics
- **Severity**: HIGH
- **Exact section it hits**: Phase-3 4.2; inherited claim against phase-2 5.3
- **Confidence**: High
- **The problem**: Enrollment accepts provider JWT + `{device_pubkey, device_name, platform}` and says “enrollment binds pubkey only.” There is no signed challenge proving possession of the device private key, no stated uniqueness rule for duplicate pubkeys, and no atomic provider-subject → account-id creation rule. Concurrent first logins could split one provider subject into two accounts; duplicate pubkey enrollment can confuse removal/re-enrollment semantics.
- **Evidence**: Phase-2 5.3 explicitly says enrollment requires a device-held secret so the cloud cannot self-enroll a device. Phase-3 4.2 does not specify that mechanism.
- **Concrete fix**: Enrollment must be challenge-response: AccountDO/Worker issues nonce over account/provider/device metadata; device signs with its static private key. Add transactional uniqueness for provider subject → account_id and for active `(account_id, device_pubkey)` / `device_instance_id`. Define duplicate/re-enroll behavior explicitly.

## Finding P3-H2: Device tokens are bearer-only control-plane authority
- **Severity**: HIGH
- **Exact section it hits**: Phase-3 4.2, 5.1, 
- **Confidence**: High
- **The problem**: A stolen device token can impersonate the device to rendezvous: open/replace the live control WS, publish candidates, send `connect_request`, request relay grants, and receive offers. Noise blocks plaintext/tool impersonation, but control-plane impersonation still enables DoS, candidate poisoning, LAN probing, and stale-token races.
- **Evidence**: 4.2 makes the token “opaque bearer” and day-to-day rendezvous auth;  admits stolen token = rendezvous impersonation, but treats “cannot speak Noise” as sufficient.
- **Concrete fix**: Bind every control WS to both token and device key: `hello` must include a server nonce signed by the device static key. Fence one live session per device with session generation. Add token IDs, rotation epochs, explicit old-token grace, immediate WS close on revoke/rotate, and per-message monotonic sequence/nonces for replay protection.

## Finding P3-H3: Offer-to-unverified plus self-reported candidates creates LAN SSRF/scan surface
- **Severity**: HIGH
- **Exact section it hits**: Phase-3 4.3, 5.2, 5.3,  Q2/Q4
- **Confidence**: High
- **The problem**: Same-account devices can signal each other before verification, and candidates are self-reported. An account thief or malicious sibling can enroll a rogue device, send `connect_request`, and cause a target to dial attacker-supplied LAN/public addresses before trust is established. Noise prevents subc plaintext, but the target still emits TCP SYNs/Noise bytes into internal networks and leaks timing.
- **Evidence**: 5.2 allows self-reported `lan` candidates; 5.3 relays `connect_offer {from, candidates}`;  Q4 says current design allows offer-to-unverified.
- **Concrete fix**: For unverified peers, relay only a `pairing_intent` notification. Do not disclose/dial candidates or issue relay grants until the target user accepts the pairing prompt. Block loopback/link-local/multicast/metadata ranges, rate-limit candidate attempts, cap unverified offers, and avoid returning fine-grained success timing to the requester.

## Finding P3-H4: Relay pipe-token semantics are inconsistent and under-specified
- **Severity**: HIGH
- **Exact section it hits**: Phase-3 5.3, 
- **Confidence**: High
- **The problem**: 5.3 shows a singular `pipe_token` “issued to both sides,” while  says “per-side `pipe_token`, single-use, short TTL.” The design does not specify audience binding, atomic consume, replay handling, or whether device X can redeem device Y’s token.
- **Evidence**: The two sections contradict on singular vs per-side tokens and provide no validation algorithm.
- **Concrete fix**: Mint two distinct per-side capabilities containing `pipe_id`, `account_id`, `side_pubkey`, `peer_pubkey`, `role`, `jti`, `exp`, and RelayDO id. RelayDO must atomically consume each `jti` once, reject wrong-side redemption, reject same pubkey on both sides, enforce short server-clock TTL, and cancel grants on revocation.

## Finding P3-H5: “Pubkey tie-break as phase 2” is unsupported; double-win sessions are unresolved
- **Severity**: HIGH
- **Exact section it hits**: Phase-3 5.4; inheritance checked against phase-2
- **Confidence**: High
- **The problem**: Phase-3 relies on a pubkey tie-break “as phase 2,” but the phase-2 design does not define a network dialer tie-break. Simultaneous connect attempts can produce two successful carriers, e.g. TCP and relay, with each side choosing a different “first success.” Duplicate Noise sessions to the same peer can race catalog registration and loopback teardown.
- **Evidence**: Only phase-3 5.4 mentions the tie-break. Phase-2 defines one loopback connection per `(peer, remote module)` and partition cleanup, but not candidate-election mechanics.
- **Concrete fix**: Define a connection election protocol: deterministic dialer/listener by pubkey, `connection_attempt_id`, transport priority, and loser-close rules. Both sides must converge on the same winning session before registering/replacing loopback connections. Add simultaneous-dial tests where TCP and WS both complete.

## Finding P3-M1: WebSocket record framing has ambiguous mismatch behavior
- **Severity**: MEDIUM
- **Exact section it hits**: Phase-3 
- **Confidence**: High
- **The problem**: “One fed record per WS message” plus retained 4-byte length prefix is only safe if mismatch behavior is normative. The design does not say what happens if a WS message contains multiple records, a partial record, or a prefix length different from the WS payload length.
- **Evidence**:  states the framing shape but no parser invariants or close/error rules.
- **Concrete fix**: Require binary WS payload length exactly `4 + prefix_len`; prefix ≤ 16 MiB; no extra bytes; no cross-message parser buffering; close connection on mismatch with a typed protocol error.

## Finding P3-H6: Relay idle teardown can be misclassified as peer partition
- **Severity**: HIGH
- **Exact section it hits**: Phase-3   inherited from phase-2 6.1/6.2
- **Confidence**: Medium-high
- **The problem**:  says idle timeout tears the pipe down and peers reconnect via fresh grant;  says keepalive/reap unchanged and relay partitions classify like TCP. If RelayDO idle timeout, hibernation wake, or grant reacquisition exceeds the 3× reap window, a healthy relayed peer can be declared partitioned, closing loopback routes and surfacing spurious GOODBYE/`OutcomeUnknown`.
- **Evidence**: Phase-2 6.2 supports the reaper as authoritative partition classifier; it does not make expected relay idling equivalent to TCP failure without timer constraints.
- **Concrete fix**: Specify timer inequalities: relay idle timeout must exceed `3× keepalive + cold-wake + grant budget`, or intentional relay dormancy must enter a separate “dormant, not partitioned” state. Recovery must query the serving ledger across a fresh pipe. Add tests for relay teardown mid-mutating-effect.

## Finding P3-H7: Present-but-down rendezvous fallback is not airtight
- **Severity**: HIGH
- **Exact section it hits**: Phase-3 5.5, 
- **Confidence**: High
- **The problem**: The doc guarantees `[rendezvous]` absent equals phase-2 behavior, but not that rendezvous configured-but-down falls back cleanly. A control-client boot path that waits for AccountDO, token refresh, or registry snapshot could brick otherwise reachable static peers.
- **Evidence**: 5.5 only specifies absent behavior and static `addr` as a candidate;  says static profiles remain fallback but gives no failure-mode algorithm.
- **Concrete fix**: Load static profiles first and dial static/TCP independently of rendezvous. Rendezvous startup must have bounded deadlines and circuit breakers. Account/control failures must not alter verified static peer routability. Add tests: Worker down, DO slow, token expired, and static peer call still succeeds.

## Finding P3-M2: Registry candidate freshness/versioning is missing
- **Severity**: MEDIUM
- **Exact section it hits**: Phase-3 5.2, 5.3, 5.4
- **Confidence**: Medium-high
- **The problem**: Candidate lists have `last_seen_ms` but no candidate generation, expiry, or replay protection. A stale `connect_offer` can race against online flaps or changed addresses, leading to dead/wrong dials and unnecessary relay fallback.
- **Evidence**: `connect_offer` carries `{from, candidates}` only; no generation/nonce/timestamp is specified.
- **Concrete fix**: Add server-issued `candidate_generation`, `observed_at`, `expires_at`, and offer nonce. Ignore expired/stale offers; re-fetch before relay fallback after direct failures; coalesce flapping updates.

## Finding P3-M3: AccountDO signaling quotas/backpressure are absent
- **Severity**: MEDIUM
- **Exact section it hits**: Phase-3 5.1, 5.3; phase-2 5.5 only covers relay abuse
- **Confidence**: High
- **The problem**: One AccountDO holds all live WS, registry, and signaling for an account. The design specifies relay quotas in phase-2, but not rendezvous quotas. A malicious/stolen-token device can flood `connect_request`, `relay_open`, and candidate updates, delaying all sibling devices in the account.
- **Evidence**: 5.1 centralizes per-account state; 5.3 lists unmetered signaling ops.
- **Concrete fix**: Add per-account/per-device/target leaky buckets, max pending offers/grants, candidate-update coalescing, priority for verified peers, and metrics/alerts for DO queue depth and hibernation wake latency.

## Finding P3-L1: Security TTLs and `last_seen` need server-clock semantics
- **Severity**: LOW
- **Exact section it hits**: Phase-3 4.2, 5.2, 
- **Confidence**: Medium
- **The problem**: Token rotation/expiry, relay grant TTL, and `last_seen_ms` are mentioned without saying whose clock is authoritative. Device wall-clock skew or rollback must not extend token/grant validity or falsify freshness.
- **Evidence**: 5.2 includes `last_seen_ms`;  says “short TTL”; no clock rule is stated.
- **Concrete fix**: All security TTLs and `last_seen` values must be stamped and evaluated by Worker/DO server time. Device timestamps are display-only. Include server-issued expiry in grants/offers.

## ANSWERS TO THE FIVE OPEN QUESTIONS

1. **Device-token custody**: Put device tokens in the credentials vault / OS keychain now. `0600` beside the device key is acceptable only as a temporary dev fallback, not v1 product posture, because token theft gives full rendezvous impersonation.

2. **Self-reported registry candidates**: Harden before build. Block dangerous address ranges, rate-limit dials, add candidate generations/expiry, server-stamp public candidates, and do not dial unverified peers’ LAN candidates until user consent.

3. **Relay pipe lifetime**: Use per-connection, per-side, single-use grants; do not issue standing reusable pipe tokens. Keep an active relay pipe alive with normal keepalives, with idle timeout greater than the reap window. Reconnect-per-keepalive churn is not acceptable.

4. **`connect_offer` to unverified targets**: Automatic candidate-bearing offers should require verification. For pairing UX, allow only a loud `pairing_intent` to unverified devices; no candidates, relay grants, or dialing until the target user accepts.

5. **WS hibernation latency**: Acceptable only with explicit budgets. Cold wake must not count against the ~2s candidate dial timeout. Use a separate signaling deadline, measure p95/p99 wake latency, and keep AccountDO warm while an account has active pairing/dial UI.

## OVERALL VERDICT

**NO-GO as written.** The primary must-fix blocker is **P3-B1 revocation/session/grant enforcement**: device removal must become a data-plane invariant, not merely token revocation plus best-effort registry broadcast.

Before implementation, I would also require fixes for enrollment proof-of-possession, device-token channel binding, relay grant audience binding, deterministic candidate-session election, and present-but-down rendezvous fallback. Overall confidence: high for the documented gaps; medium where timing behavior depends on implementation choices not yet specified.