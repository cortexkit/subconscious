# ADVERSARIAL DESIGN REVIEW — subc Federation Phase 3 (DRAFT v1)

Scope: I read both `docs/subc-federation-phase3-design.md` (DRAFT v1) and `docs/subc-federation-design.md` (v4.1, shipped phase 0-2) end-to-end. Where phase 3 asserts an inherited property, I checked the phase 2 doc for the supporting clause. Inheritance claims that do not survive the check are findings.

Verdict at the bottom: **NO-GO**. The design has shape but ships at least three BLOCKERs and a flock of HIGHs, all of them in the *implementation* of the locked decisions (which is exactly what is in scope).

---

## F1 — BLOCKER: Phase-3 contradicts the phase-2 6.1 effect-ledger mechanics with "Noise session resumption = ordinary re-handshake, cheap"

- **Severity**: BLOCKER
- **Location**:  (Relay), line 203-204
- **Confidence**: HIGH
- **Issue**: The phase-2 doc (v4.1) specifies the at-most-once state machine in 6.1 (lines 196-207): intent fsync BEFORE the first network write; outcome fsync BEFORE the consumer sees the reply; on restart, recovery reconciliation queries the SERVING ledger for every `sent`-without-outcome effect. The effect_id carries a stable incarnation epoch. The reaper closing a peer's loopback connection is the partition classifier (line 214). Phase 3  adds idle teardown of the RelayDO pipe and says "peers reconnect via a fresh grant — Noise session resumption = ordinary re-handshake, cheap." This combines into a correctness gap:
  1. The reaper is fed by per-peer keepalive (phase 2 6.2 line 213). With a relay pipe, the **carrier-level** liveness is two things: (a) the WS to the RelayDO, and (b) the peer at the other end. Idle teardown is (a) only — it is NOT a peer partition. The reaper must NOT classify a relay-idle as a peer partition; today nothing in the doc says how the reaper distinguishes "pipe torn down, peer still alive" from "peer partitioned." The phase-2 reaper has no concept of "torn down pipe, fresh pipe to come."
  2. A "fresh grant" carries a fresh Noise IK handshake — that is a *new* cryptographic session. Phase 2 6.1's recovery reconciliation keys entirely off the (origin_device_pubkey, incarnation_uuid, seq) effect_id and the serving-side dedup ledger. The Noise session is irrelevant to the dedup ledger, but the doc's claim of "cheap" is also wrong: each reconnect is a full DH (X25519) on both sides plus a round trip through the relay. At keepalive cadence + per-idle-teardown reconnect, that is non-trivial for an idle connection that just woke up.
  3. The phase-2 inheritance claim "relay-path partitions classify exactly like TCP partitions" (line 195) does not survive the check: the phase-2 doc's partition classifier looks at per-peer keepalive and closes that peer's loopback connections. It is silent on WS/relay carriers. "Exactly like" is asserted but not inherited.
- **Evidence**: phase-2 6.1 lines 196-207 (effect-ledger mechanics keyed on effect_id, not Noise session lifetime); phase-2 6.2 lines 213-214 (reaper semantics); phase-3  line 194-195 and  line 203-204.
- **Fix**:
  - Define a separate "pipe state" abstraction in the fed-module: a logical peer-connection survives a relay pipe teardown. The reaper fires on the logical connection's keepalive (fed-module-level, not carrier-level). A relay-idle is a TRANSPORT EVENT, not a partition event; it triggers a fresh `relay_open` and a Noise IK re-handshake, but the reaper's clock and the effect ledger's incarnation epoch are unaffected.
  - Move the "cheap" claim out: a full IK re-handshake over a relay is O(1 DH + 1 RTT). It is acceptable, not cheap. State so. If "cheap" is a hard product requirement, add a Noise resumption token persisted across the pipe-idle window (an application-level 0-RTT resumption) — but that is non-trivial design and must be specified.

## F2 — BLOCKER: `connect_offer` allowed to unverified targets breaks the threat-model claim for the pairing UX window

- **Severity**: BLOCKER
- **Location**: 4.3 (F4 locked), 5.3 (signaling ops), open question 4
- **Confidence**: HIGH
- **Issue**: 4.3 claims "A cloud/account compromise therefore yields metadata + the ability to *offer* a rogue device for pairing — it cannot make any existing device trust the rogue one, and the rogue device's verify-code will not match anything the operator's real devices display." 5.3 lists `connect_request → connect_offer` to an arbitrary `to: pubkey` (no `verified:true` precondition). Open question 4 confirms the current design is "allowed, loud."
  The phase-2 ceremony (5.3 line 172 of v4.1) gates first contact with a *code comparison*. The phase-3 doc, however, never says how the CK app surfaces the ceremony to the target for an incoming `connect_offer` from a previously-unknown pubkey. The most common UX failure is "Accept/Deny" buttons instead of a code. With Accept/Deny, the operator clicks Accept → the rogue device becomes routable. The verify-code protection is a property of the **rendering**, not of the protocol. The doc has nothing enforcing that rendering. "Loud" join events (line 110) are noise, not ceremony.
  Compounding: 5.3's `connect_request` is relayed to the target. The target is presented with an offer from a pubkey it has never seen. If the target's CK app renders "Pair with 'unknown device', pubkey 0xAB…? [Accept] [Deny]" — which is the easy implementation — the operator is one tap from trusting the rogue device. The doc has no contract on this rendering.
- **Evidence**: phase-2 5.3 line 172 ("non-routable … until the out-of-band verification code is compared and confirmed in the CK app on both ends"); phase-3 4.3 lines 104-110, 5.3 lines 150-152, open question 4 line 261-263.
- **Fix**:
  - State explicitly that `connect_offer` to an unverified pubkey forces the **SHA-256 safety-number display** on the target, computed independently by both sides from both static keys. The accept gesture is a single button labelled "Codes match"; there is no Accept/Deny without a code.
  - Require the target's accept message to include a *signature* over the offered safety number using the target's static key, and the offerer to verify it; this catches an attacker who rewrites the safety number post-offer.
  - Add an explicit `verified:false` flag on every effect on the discovered peer until the ceremony completes; the fed-module refuses to forward tool calls (not just refuses routing) until verification, not just refuses binding.

## F3 — BLOCKER: Pipe-token model is underspecified in a way that almost certainly allows cross-peer and cross-pipe redemption

- **Severity**: BLOCKER
- **Location**: 5.3 line 153;  line 199-200
- **Confidence**: HIGH
- **Issue**: 5.3: `relay_open {to: pubkey}` returns `relay_grant {relay_url, pipe_id, pipe_token}` "issued to both sides" ( line 200).  "per-side `pipe_token`, single-use, short TTL." Three problems compound:
  1. **Who can consume whose grant?** The doc does not say whether `pipe_token` is bound to a specific device pubkey. If it isn't, device X receives a `relay_grant` for `pipe_id=P`, and device X redeems it to open its end; but Y's grant for the SAME `pipe_id=P` is also a `pipe_token` — if X can also present Y's `pipe_token` to the RelayDO, X can masquerade as Y on the relay. The doc must bind each `pipe_token` to `(pipe_id, device_pubkey)` at mint time and enforce on consume.
  2. **"Single-use"** for the *consume* step is specified, but `pipe_id` is the pipe itself — a WS hub at the RelayDO. The RelayDO accepts TWO consumes per pipe (one per side). What "single-use" means here is that each `pipe_token` is redeemed once. But the doc does not specify: (a) what happens if a side redeems, then disconnects, then tries to re-redeem the same `pipe_token` (probably rejected — good); (b) what happens if the pipe is fully formed and idle-tears down, can the same `pipe_id` be reused with a fresh grant? The doc says "peers reconnect via a fresh grant" but does not say whether the `pipe_id` is a new value or whether the old `pipe_id` is retired and a new one minted.
  3. **RelayDO auth-before-resource** (inherited from phase 2 5.5 line 184) is stated at the high level but the doc does not say: the RelayDO must verify the connecting WebSocket presents a `pipe_token` whose binding matches the WS's authenticated device identity (which is the device token from 4.2). Otherwise an attacker who steals one `pipe_token` and one device token can ride any pipe.
- **Evidence**: phase-3 5.3 line 153,  lines 199-200,  line 220; phase-2 5.5 line 184 ("auth-before-resource (a connection proves device identity before any forwarding state is allocated)").
- **Fix**:
  - Specify `pipe_token = HMAC(pipe_id_key, pipe_id || device_pubkey || exp || nonce)`; the RelayDO looks up `pipe_id`, verifies the HMAC under its `pipe_id_key`, and refuses if `device_pubkey` in the token does not match the WS's authenticated device identity (which itself is bound to a device token presented at WS upgrade).
  - State explicitly: on idle teardown, BOTH sides must present a NEW `pipe_token` to a NEW `pipe_id`. The old `pipe_id` is atomically retired. Cross-pipe continuity is a fed-module concern, not a RelayDO concern.
  - On re-handshake, require the device token to be re-presented (a stale `pipe_token` alone is insufficient).

## F4 — BLOCKER: Rendezvous-down fallback path is asserted but not engineered, and the "candidate-priority dialing" claim depends on it

- **Severity**: BLOCKER
- **Location**: 5.1, 5.4, 5.5
- **Confidence**: HIGH
- **Issue**: 5.5 states "Absent → phase-2 behavior exactly (static profiles keep working forever; the WAN test rig never needs an account)." This is the inheritance claim. But the candidate-priority dialer in 5.4 — lan → public → relay — needs candidates to come from *somewhere*. If the `[rendezvous]` section is present but the rendezvous is unreachable (Worker down, account DO cold-waking, network partition), the candidate list is empty or stale. Phase 2 has no notion of "candidate list from the cloud"; the fed-module's profile has static `addr` lines, and that's what it dials.
  The phase-2 design's partition classifier (line 214) closes the per-peer loopback connection on a missed keepalive. With a rendezvous present-but-down, every per-peer connect goes through the rendezvous. The fed-module's reaper sees the carrier's WS to the rendezvous as alive (it's still up) but signaling responses time out. The reaper's input is the per-peer keepalive to the PEER, not the rendezvous. So a rendezvous-down state is not classified as a partition; it's an indefinite "I have no candidates" state. The design asserts "static profiles remain the availability fallback" but does not specify the code path that says: "if rendezvous is unreachable, dial the static `addr` from the profile" — let alone how the fed-module decides the rendezvous is "down" vs "empty."
  A simpler attack: an attacker who can keep the rendezvous WS open (e.g. by holding a valid device token) but never relay signaling — the static-fallback path is fine, but the candidate-priority path silently no-ops. Worse: if the rendezvous is held open but `connect_request` is dropped on the floor (signaling abuse), the operator gets a hung UX with no diagnostic.
- **Evidence**: phase-2 5.5 line 184 and 6.2 line 214 (reaper is per-peer, not per-rendezvous); phase-3 5.4 line 161-169 (candidate priority), 5.5 line 173-180 (rendezvous section + absent behavior),  line 225-226 ("static profiles remain the availability fallback").
- **Fix**:
  - Specify a **rendezvous-dead timer** on the fed-module: after N seconds of rendezvous-unresponsive, mark rendezvous unavailable, fall back to profile's static `addr` lines as the only candidate. Re-attempt rendezvous on a slow background schedule. State the timer (e.g. 5s; document rationale).
  - Make signaling timeouts explicit and short. `connect_request → connect_offer` is delivered to the rendezvous; if no `connect_offer` reaches the peer within T1, retry once; if no `connect_accept` within T2, fail. T1, T2 must be tuned against WS-hibernation cold-wake latency (open question 5).
  - Add a `rendezvous` status field to the `registry_delta` so the fed-module can distinguish "registry says zero candidates" from "registry unreachable."

## F5 — HIGH: Re-enrollment after `revoked_by_account` is underspecified and creates a TOFU-pinning hole

- **Severity**: HIGH (not BLOCKER because the phase-2 ceremony's safety number still gates it, but the spec is missing a contract)
- **Location**: 4.2 line 95-98
- **Confidence**: HIGH
- **Issue**: Phase-2 5.3 (line 173-174): "Rotation ceremony: a legitimate key rotation must be signed by the old key (tombstone chain) or confirmed via an already-verified device / manual re-pairing; any other key change presents as first contact (non-routable + code comparison)." Phase-3 4.2: "Device removal: … profile entry flips to `revoked_by_account`, non-routable, requires re-pair if re-enrolled."
  The re-enroll case is not specified. Two distinct scenarios:
  1. **Same device, same key** (user lost access, got it back): the device re-enrolls with the SAME pubkey. Should the rendezvous re-bind the existing device record, or reject because the device was removed? Today the doc does not say. If it re-binds, the operator's other devices still have the pubkey pinned, but the device record in the AccountDO is fresh. A compromised cloud can re-bind to a stolen pubkey if it can produce a WorkOS login.
  2. **Same device, new key** (key loss / re-key): the device re-enrolls with a NEW pubkey. This is a key rotation, which phase 2 says must be confirmed via an already-verified device. The phase-3 doc says "requires re-pair" but does not say: (a) does the rendezvous notify other devices of the new key? (b) does the operator have to re-verify the new key on each existing device? (c) what if the new key collides with an existing pubkey? (d) what if the new key is the rogue-device's key (a cloud-introduced substitution)?
  The phase-2 design's "tombstone chain" inheritance is *partially* honored ("requires re-pair") but the cloud-side story is missing: the rendezvous has no protocol to chain an old key to a new key. A rogue device with WorkOS access and a fresh pubkey can replace the old pubkey in the AccountDO and present itself as "re-enrolled" to existing peers, who then see a new pubkey to compare against (which won't match the old one they had pinned).
- **Evidence**: phase-2 5.3 lines 171-174; phase-3 4.2 lines 95-98.
- **Fix**:
  - Specify the re-enroll flow: when a device re-enrolls, the AccountDO must publish a `registry_delta` containing `(device_pubkey_old, device_pubkey_new, tombstone_signed_by=old_or_verified_device)`. Other devices receiving the delta must:
    - Verify the tombstone is signed by the OLD key OR by a currently-verified device (not just a sibling device that is itself unverified).
    - Treat the new key as **first contact** until the operator re-verifies — even if they previously verified the old key. This is what phase-2 says; phase-3 must restate it.
  - For same-key re-enroll after removal: the AccountDO must require a WorkOS-side proof of access (not just a device token — the device token was REVOKED). WorkOS is the only root of identity for enrollment. The new device token binds to the existing pubkey.

## F6 — HIGH: Candidate self-reporting is a SIBLING-DEVICE trust hole

- **Severity**: HIGH
- **Location**: 5.2 line 126-143, open question 2
- **Confidence**: HIGH
- **Issue**: 5.2 lets a device self-report `candidates`, including the `lan` candidate. Open question 2: "any hardening needed against a malicious sibling device on the same account lying about its LAN addr?" The current answer is "it can only redirect its own inbound dials; Noise pins identity regardless."
  The answer is wrong in two ways:
  1. **A sibling CAN redirect the OPERATOR's outbound dials.** If the operator on device A dials the `lan` candidate published by sibling B (which says `192.168.1.34:7841` but really is `attacker.local:9999`), the dial is from A to attacker. The Noise handshake then fails — the attacker doesn't have B's static key. So far so good. But:
  2. **A sibling CAN lie about the operator's `lan` candidate on behalf of OTHER operators.** If the AccountDO trusts self-reported candidates without rate-limiting or sanity-checks, a compromised sibling can flood the registry with many fake candidates across all its peers, and the next dial to any peer that uses the registry as primary source goes to attacker-controlled infra. Noise handshake fails, but the dialer learns `attacker.local` exists, and if the attacker also has B's static key (from a separate compromise), the dialer completes a Noise session with the WRONG peer. The verify-code display on A's side is for the claimed pubkey (B's), but the dial is to the attacker's endpoint, which is using B's real key. The code compares and matches. The operator has no signal that the dial is to attacker infra.
  This is a real, exploitable false-routing attack. The "noise pins identity regardless" rebuttal is wrong because the dial is E2E — the Noise session is with whoever owns the static key at the address it dials. The address is the variable.
- **Evidence**: phase-3 5.2 lines 126-143, 5.4 lines 161-169; phase-2 2.2 (data plane is E2E by design — meaning the dial endpoint IS the security boundary).
- **Fix**:
  - Bind candidates to (account, device_pubkey) and pin: the *first* time a candidate is observed for a device, the operator (on each other device) sees a "first-seen candidate" prompt and must accept before future dials use it. Subsequent candidates of the SAME kind must match the same pattern (e.g. IP/24 for public, IP/octets for LAN) or trigger the same prompt.
  - For the `lan` candidate specifically: dial the local-network BROADCAST first (mDNS or a small LLMNR-style query for the device_pubkey fingerprint) and verify the responding endpoint holds the right static key. Only use a self-reported `lan` candidate if the broadcast confirmation succeeds. The Noise handshake already does this cryptographically; what is missing is binding the candidate to "this is the network address where I've SEEN this key" rather than "the device said this is its address."
  - Server-observed `public` candidates are already stronger (the Worker stamps from observed source IP) but note: the `public` candidate is also self-reportable from the device — the Worker must ALWAYS overwrite, never accept, the `public` from the device payload.

## F7 — HIGH: Device-token revocation vs in-flight signaling is racy

- **Severity**: HIGH
- **Location**: 4.2 line 95-98
- **Confidence**: HIGH
- **Issue**: The phase-2 doc 5.5 (line 184) says the relay does "auth-before-resource (a connection proves device identity before any forwarding state is allocated) and enforces per-account/per-device quotas + rate limits, so an attacker can't amplify or exhaust the relay." Phase-3  (line 222): "Stolen device token | impersonate that device to the rendezvous (signaling + registry) | Cannot speak Noise as the device (no private key); revocable server-side."
  The "revocable server-side" claim is correct, but the doc does not specify the revocation semantics for in-flight signaling. Concretely:
  1. Attacker holds a stolen device token. The legitimate operator revokes the token. Between revoke and the AccountDO propagating the revoke to all in-flight sessions, the attacker can still:
     - Complete a `relay_open` against a victim device (the grant is then issued to BOTH sides, and the legitimate device accepts a relay pipe from the attacker).
     - Re-issue signaling (push a `connect_offer`).
  2. The device token has rotation, but rotation is not specified. If a device token is rotated while a peer holds a stale token, the peer's next `hello` is rejected — but the doc does not say when. If rotation is "operator-initiated via CK app," there is a window where the new token is delivered to the device but the AccountDO has not yet marked the old one revoked; the attacker can race this window.
- **Evidence**: phase-2 5.5 line 184; phase-3 4.2 lines 86-99,  line 222.
- **Fix**:
  - Specify token revocation as: AccountDO maintains a `revoked_token_set`; on every `hello`, the WS is checked against current valid AND against a small "recently revoked" window (e.g. last 5 minutes). On hit, the WS is closed and an out-of-band notification is sent to the legitimate device.
  - Token rotation: rotation produces a NEW token AND atomically invalidates the OLD token. The new token is delivered to the device over the existing authenticated channel; the old token is invalid from that moment. There is no "two valid tokens" window.
  - `relay_open` issued while a token is later revoked: the RelayDO must check token validity on every WS frame, not just at upgrade. If revoked, drop the WS and tear down the pipe.

## F8 — HIGH: Signaling abuse — no nonce, no timestamp, no per-account/per-pubkey rate limit

- **Severity**: HIGH
- **Location**: 5.3
- **Confidence**: HIGH
- **Issue**: 5.3 lists the signaling ops: `hello`, `registry_delta`, `connect_request`, `connect_offer`, `connect_accept`, `relay_open`, `relay_grant`. None carries a nonce or timestamp.  (line 220) says "Signaling abuse: connect_request floods" is not in the threat model — the doc notes only that "wrong addresses" is possible.
  Concrete attacks:
  1. **Replay.** A `connect_request` captured by an attacker (e.g. on-path) is replayed N minutes later. The rendezvous delivers a duplicate `connect_offer` to the target. The target, having already paired with the offerer, sees "connect_offer" and either silently completes the dial (no ceremony check) or shows a duplicate UI. The doc does not say.
  2. **Flood.** A compromised device token (or a stolen WorkOS account) issues `connect_request` to every device in the account repeatedly. The target's CK app gets spammed. The relay gets grinded.
  3. **Cross-account injection.** A compromised cloud can issue `registry_delta` to any account's devices (no per-account or per-device signing). The doc does not specify that registry deltas are signed by the AccountDO. If they are not, a compromised Worker can lie.
- **Evidence**: phase-3 5.3 lines 146-159; phase-2 5.5 line 184 mentions quotas but only for the relay, not the rendezvous.
- **Fix**:
  - Add a `nonce + timestamp + signing_account_id` envelope to every signaling message. The AccountDO signs outbound `registry_delta` and `connect_offer` with an account-binding key (rotated; the device can verify against the WorkOS-issued account token or a separate account pubkey).
  - Rate limit `connect_request` per (account, source device pubkey) AND per (account, target device pubkey). Default: 10/min per source, 30/min per target.
  - `connect_offer` MUST carry the offerer's current `registry_seq` or equivalent freshness tag so the target's fed-module can reject stale replays.

## F9 — MEDIUM: AccountDO is a hot spot and the design doesn't bound it

- **Severity**: MEDIUM (operational, not security)
- **Location**: 5.1
- **Confidence**: HIGH
- **Issue**: "one per account" is the design. For a small user base (paid tier, individual), this is fine. For any team / shared-account topology later, one DO funnels all signaling+registry. Cloudflare DO throughput is per-DO; a single DO is a single-threaded state machine. The design's "via Cloudflare" inheritance is OK for v1 individual users; the doc should say so.
- **Fix**: Document the per-account throughput assumption (signaling ops/sec/account) and the failure mode if exceeded (backpressure on the WS, queue depth, dropped messages). Defer sharding/team-DOs to phase 4+.

## F10 — MEDIUM: WS-hibernation cold-wake will blow signaling timeouts

- **Severity**: MEDIUM
- **Location**: 5.1, 5.4, open question 5
- **Confidence**: HIGH
- **Issue**: 5.1: "hibernation-friendly — WS hibernation keeps idle accounts at ~zero cost." 5.4: "short per-candidate timeout (~2s)." Open question 5: "acceptable to pay a cold-wake round-trip on first signal to an idle account?"
  A Cloudflare DO WS cold-wake is documented (in Cloudflare's own docs) to be tens to hundreds of ms in the best case, seconds in the worst case. A 2s per-candidate timeout will routinely be blown. The 2s number is not justified.
- **Fix**:
  - Bump the signaling timeouts (e.g. T1=connect_request→connect_offer=5s; T2=connect_offer→connect_accept=10s) to absorb cold-wake jitter. Document the choice.
  - Add a "wake-warm" heartbeat from device to AccountDO on application-level events (any control-plane activity) so the DO is warm during normal usage.
  - Probe-and-pin: keep the WS warm when the device is in active use; only let it hibernate after N minutes of zero activity.

## F11 — MEDIUM: `revoked_by_account` re-pair path — pubkey re-enrollment after revocation bypasses the cloud's idea of the operator's intent

- **Severity**: MEDIUM
- **Location**: 4.2, 5.5
- **Confidence**: MEDIUM
- **Issue**: The doc says "Device removal: … profile entry flips to `revoked_by_account`, non-routable, requires re-pair if re-enrolled." This is correct for the same-account peer. But the AccountDO has no record that the operator explicitly authorized the re-enroll. A compromised WorkOS account can re-enroll any pubkey. Phase 2 5.3 (line 174) says the residual is "a cloud-tier user who skips the code comparison is trusting the cloud at introduction time." Phase 3 inherits this. But the re-enroll case is *not* an introduction; it is a re-introduction after removal. The operator who removed the device is no longer presented with a "compare codes" UX on the OTHER devices — they are presented with a "the device is re-enrolled, restore?" prompt. The doc does not specify which.
- **Fix**:
  - On re-enroll, the operator's OTHER devices must go through the FULL code-compare ceremony, exactly as for first contact. Not a restore prompt.
  - The AccountDO must publish a "device was re-enrolled" event that the operator's CK app surfaces LOUDLY, separately from "device was added" (which is a first-contact event).

## F12 — MEDIUM: Device-token custody on disk is exposed to an entire class of local attacks

- **Severity**: MEDIUM
- **Location**: open question 1, 4.2 line 88-91
- **Confidence**: HIGH
- **Issue**: Open question 1: "0600 file next to device key — sufficient for v1, or fold into the credentials vault immediately?" The 0600 file is plaintext-readable by any process running as the same user, and any process with the same UID on a misconfigured host. The fed-module's device key (Noise static) is presumably protected by a `reserved`+`launch_nonce` mechanism, so it can't be read by an arbitrary process — but a device token sitting next to it as plaintext is a strictly lower bar. The doc's threat model says "stolen device token | impersonate that device to the rendezvous" — the bar to steal is the same as the bar to read the device key file, which is the bar to read any file the user can read.
  This is a real issue because the **rendezvous** is the most attractive target — it carries the device graph and is the relay-grant authority. A device token is enough to make the rendezvous do useful work for the attacker (signaling abuse, registry manipulation, `relay_open` grinds).
- **Fix**:
  - For v1: store the device token ENCRYPTED at rest, keyed by a per-host key derived from the device key's static secret (or from a separate derived host-secret). The fed-module decrypts on use; the on-disk form is opaque to other processes.
  - For phase 4 or sooner: fold into the credentials vault (per `docs/cortexkit-credentials-contract.md` — which I have not loaded in detail but is referenced as the vault contract). State the choice in the doc.

## F13 — MEDIUM: WS carrier framing — "one fed record per WS message" is asserted but not enforced

- **Severity**: MEDIUM
- **Location**:  line 187-191
- **Confidence**: HIGH
- **Issue**:  "one fed record per WS message; the 4-byte length prefix is redundant inside a message framing but kept identical so record parsing is carrier-agnostic and golden vectors hold." The 4-byte length prefix is REDUNDANT inside a single-record-per-message framing — but the doc keeps it. The receiver's parser is fed "exactly one record per message" — but on a multi-record or zero-record message, the doc says nothing.
  Concretely:
  1. If a malicious relay (or a buggy fed-module) emits a WS message with TWO records concatenated and the redundant length prefix points to the first one, the receiver reads one record and discards the rest (lossy) or errors (no spec).
  2. If the redundant length prefix disagrees with the WS message length, the receiver has no spec.
  3. The "kept identical so golden vectors hold" rationale is weak: a parser that asserts length-prefix-equals-WS-message-length is stricter than a parser that asserts length-prefix-equals-record-length-only. The doc's spec is ambiguous.
- **Fix**:
  - Specify the receiver's contract: on a WS message of N bytes, expect the 4-byte length prefix to be exactly N-4. If it differs, close the carrier with a framing error. The redundant prefix is then a sanity check, not redundancy.
  - On a multi-record or zero-record message: reject with a framing error. No lossy behavior.

## F14 — MEDIUM: "Carrier-blind keepalive" inheritance — relay teardown is NOT a partition, but the reaper will treat it as one

- **Severity**: MEDIUM
- **Location**:  line 192-195, 
- **Confidence**: HIGH
- **Issue**:  "Keepalive cadence and 3× reap window unchanged; relay-path partitions classify exactly like TCP partitions." The phase-2 6.2 (line 214) reaper "closes that peer's loopback connections" on a missed keepalive. If the fed-module treats a relay pipe teardown as a missed keepalive (because the carrier's WS dropped), it closes the peer's loopback connections — even though the peer's fed-module may be about to open a fresh pipe via a fresh `relay_grant`. The "OutcomeUnknown" storm on the consumer side is the result.
  The phase-2 reaper is per-peer, not per-carrier. The phase-3 doc needs to specify: a relay pipe teardown is a TRANSPORT-LEVEL event, not a PEER-LEVEL event. The reaper must NOT fire on a relay teardown; it must wait for either (a) a fresh pipe to be established within a small budget, or (b) a true keepalive miss to fire.
- **Fix**:
  - Specify a "pipe-torn-down grace period" (e.g. 3× the keepalive cadence) during which the reaper is suspended for the affected peer. If a fresh pipe establishes within the grace, the reaper resets. If not, the reaper fires.
  - This grace must apply to the logical peer, not to the carrier. A subsequent relay-pipe establishment is the same peer; the reaper's "missed keepalive" counter must be reset on any successful round-trip with the peer, regardless of carrier.

## F15 — MEDIUM: Effect-ledger recovery reconciliation across a relay pipe reconnect is unspecified

- **Severity**: MEDIUM
- **Location**:  
- **Confidence**: HIGH
- **Issue**: Phase 2 6.1 (line 204): "intent durable, send unconfirmed | OutcomeUnknown; recovery queries the SERVING ledger for the effect_id: ledger hit → settle with the recorded outcome; miss + peer reachable → provably not executed → not_sent tombstone; miss + peer unreachable → stays unknown until reachable." With a relay pipe that idle-tears-down mid-effect:
  1. The origin fed-module fsynced `intent`, sent the bytes over a relay pipe.
  2. The relay idle-tears-down before the response arrives.
  3. The origin fed-module's local state: intent durable, send unconfirmed, peer "partitioned" (per the reaper bug in F14).
  4. On restart or reconciliation, the origin queries the serving ledger. The serving fed-module received the bytes, dispatched, and stored the outcome. Reconciliation succeeds.
  5. BUT: in step 3, the reaper closed the peer's loopback connection, and the consumer saw "OutcomeUnknown." The reconciliation settles the effect to "outcome" later, but the consumer's call is already terminal. The consumer's correlation between the effect_id returned at accept time and the actual settled outcome is broken.
  This is the "recovery reconciliation interact[s] badly with relay reconnects" issue called out in the prompt.
- **Fix**:
  - The recovery reconciliation MUST publish a `fed.effect_status` update that the consumer (or its harness) can query. The phase-2 doc specifies this query (line 205). The phase-3 doc must restate that the query works across relay pipe teardowns, not just TCP teardowns.
  - The consumer should query `fed.effect_status{effect_id}` on `OutcomeUnknown` and settle. The doc must specify this as the recovery contract for relay-path partitions.

## F16 — MEDIUM: Device clock assumptions — TTLs, last_seen, token expiry

- **Severity**: MEDIUM
- **Location**: 5.2 (last_seen_ms),  (pipe_token TTL), 4.2 (token rotation/long-lived)
- **Confidence**: MEDIUM
- **Issue**: The design uses:
  1. `last_seen_ms` in the registry — server-side stamped by the Worker, so the device's clock is not in the trust path here. Good.
  2. `pipe_token` "short TTL" — the device must consume the grant before the TTL. If the device's clock is skewed forward, the device thinks the token is valid but the RelayDO has expired it; the device's noise handshake fails and the effect is lost-but-marked-unretryable. If the device's clock is skewed backward, the device thinks the token is valid longer than it is; this is safe (server is authoritative).
  3. The device token is "long-lived with rotation" — the device's clock is not in the trust path (server-side expiry). Good.
  4. The device keeps a "candidates" list and may decide to use a candidate based on `last_seen_ms` (e.g. "I saw this peer 5 minutes ago, the candidate is still fresh"). If the device's clock is rolled back, stale candidates look fresh. This is a DoS amplifier: dial a dead endpoint. Not a security issue per se.
  5. The reaper's "missed keepalive" cadence is timed by the device's fed-module clock. If the device's clock is rolled back, the reaper never fires. This is a real correctness bug: a clock rollback extends the apparent liveness of a partitioned peer.
- **Fix**:
  - All server-side timestamps must be server-stamped, not client-stamped. The doc mostly does this; verify `last_seen_ms` is server-stamped (it appears to be, from 5.2 line 142 "stamped server-side by the Worker"). Make this explicit for ALL fields.
  - The reaper should be monotonic-clock based on the device, with an upper bound from server-provided timestamps. If the device's monotonic clock is unchanged but the wall clock jumps backward, the reaper continues to fire normally. State the reaper's clock source.
  - The pipe-token TTL enforcement is server-side (the RelayDO checks `exp`). Make this explicit.

## F17 — LOW: 3× reap window inheritance is asserted but the phase-2 doc doesn't say what it is

- **Severity**: LOW
- **Confidence**: HIGH
- **Issue**: Phase-3  line 194: "Keepalive cadence and 3× reap window unchanged." Phase-2 6.2 line 213-214: "Per-peer keepalive with a bounded numeric staleness window; on a missed keepalive the module marks the peer's re-exported tools unavailable rather than letting calls hang." The "3× reap window" is not stated in the phase-2 doc; it appears to be an internal fed-module constant that the phase-3 doc is now inheriting. The inheritance is a string-level claim, not a spec-level one.
- **Fix**: State the actual keepalive cadence and the actual reap window numbers (e.g. "keepalive 5s, reap window 15s"). Justify them against the relay's idle teardown (so the reaper doesn't fire on relay teardown — F14) and against the recovery reconciliation contract (F15).

## F18 — LOW: `federation_exposure` allow-list is referenced as inherited but the registry does not carry it

- **Severity**: LOW
- **Confidence**: HIGH
- **Issue**: Phase-2 2.3 (line 67): "Per-peer allow-list in the profile, authored via the CK app." Phase-3 5.2 registry entry does NOT include `federation_exposure` — only the public profile does. The fed-module reads the profile to get the per-peer allow-list, applies it after the verify ceremony. This is fine. The doc should be explicit so a reviewer doesn't think the registry is the policy source.
- **Fix**: One sentence in 5.2: "The registry carries discovery metadata only; the per-peer `federation_exposure` allow-list lives in the device's local profile (phase-2) and is applied by the fed-module after the verify ceremony, never read from the cloud."

## F19 — LOW: "Loss is acceptable" is documented, but the relitigation of "managed auth provider" should be made explicit as out-of-scope

- **Severity**: LOW (discipline)
- **Confidence**: HIGH
- **Issue**: The prompt says "managed auth provider at the edge (WorkOS), NOT self-hosted auth" is locked. The doc's threat model (line 221) names WorkOS in the table. The doc should explicitly mark "WorkOS as the v1 provider" as a LOCKED decision so the next reviewer doesn't re-litigate.
- **Fix**: Add a "Locked decisions" subsection at the top of the doc that restates the four locked items (managed auth at edge, account=discovery-only, Cloudflare hosting, relay-NOT-hole-punching, one phase gate). This is purely a discipline fix.

## F20 — LOW: Candidate list poisoning via the cloud is acknowledged but the fix is unspecified

- **Severity**: LOW
- **Confidence**: HIGH
- **Issue**: 5.3 line 156-157: "A malicious rendezvous can deny service or hand out wrong addresses; it cannot impersonate (Noise IK fails against the pinned key) and cannot read tool traffic." The "hand out wrong addresses" is the data-plane DoS — dialer wastes a connect attempt. With candidate-priority lan→public→relay, a wrong `lan` candidate wastes the 2s timeout, then a wrong `public` wastes another 2s, then the relay works. Total dial latency: 4s+ before success. For a noise session, that's a UX cliff.
- **Fix**:
  - Reduce the per-candidate timeout for known-untrusted candidates (e.g. a candidate list is hashed; if the rendezvous's delivered list does not match a previously-validated hash, treat as untrusted and reduce per-candidate timeout to 500ms).
  - Or: a small signed-claims protocol where the rendezvous's candidate list is signed by the AccountDO, and the device validates the signature before dialing.

## F21 — LOW: Static-profile fallback path is asserted to be airtight but not specified

- **Severity**: LOW (becomes a BLOCKER if F4 is fixed and this is the residual)
- **Confidence**: MEDIUM
- **Issue**: 5.5 line 175: "Absent → phase-2 behavior exactly (static profiles keep working forever; the WAN test rig never needs an account)." This is fine. But the doc does not say what happens to a previously-rendezvous-enabled device when the rendezvous is REMOVED (operator deletes `[rendezvous]` from the profile). The previously-discovered registry peers should be removed; previously-verified peers should remain verified (key-pinned). The transition is unspecified.
- **Fix**: One paragraph in 5.5 specifying the transition: removing the `[rendezvous]` section causes the fed-module to (a) close the rendezvous WS, (b) flush unverified registry peers, (c) retain verified peers, (d) re-dial the static `addr` candidates for the verified peers on next connect.

## F22 — LOW: "Device-join events are pushed loudly to all enrolled devices" — but the events are signed by whom?

- **Severity**: LOW
- **Confidence**: HIGH
- **Issue**: 4.3 line 110 and  line 221 say device-join events are "loud." Loud is the wrong word; the events need to be AUTHENTICATED. If the Worker can push a fake "join" event for a rogue pubkey, the loud UX is itself a phishing surface — operator sees "your device A is joining" and taps to confirm. The event must be signed by the AccountDO (or by a key the AccountDO attests to), and the device must verify the signature.
- **Fix**: State that `registry_delta` is signed by the AccountDO with a per-account signing key; devices maintain a public-key pin for the AccountDO established at first login (e.g. derived from the WorkOS-issued account token's JWKS or a separate per-account key distributed at enrollment).

---

## ANSWERS TO THE FIVE OPEN QUESTIONS (

1. **Device-token custody on disk (0600 file next to device key) — sufficient for v1, or fold into the credentials vault immediately?**
   **RECOMMENDATION: neither exactly as stated. For v1, encrypt the device token at rest keyed by a per-host secret derived from the device's Noise static key (e.g. HKDF(static_key, "device-token-wrap")). The fed-module decrypts on use. The on-disk file is opaque to other processes; the encryption is bound to the device, so a stolen file is useless on a different host. Folding into the full credentials vault (per `docs/cortexkit-credentials-contract.md`) is the right end state but adds coupling to a v1 that is otherwise self-contained. Do the host-bound encryption in v1; land the vault integration in a follow-up phase, not in this gate. The risk of plaintext-0600 is a real device-graph-leak surface; the host-bound encryption closes the realistic attack (other-user-process reading) without coupling.**

2. **Registry `candidates` self-reporting — any hardening needed against a malicious sibling device lying about its LAN addr?**
   **RECOMMENDATION: yes. The "noise pins identity regardless" rebuttal in the doc is wrong — Noise pins the *endpoint at the dialed address*, which is exactly what the attacker controls by lying about the address. At minimum: (a) the Worker's `public` candidate MUST always be server-stamped and never accept a client-supplied `public` value (state this explicitly); (b) the `lan` candidate SHOULD be confirmed by the dialer via a local network probe (mDNS/LLMNR query for the device_pubkey fingerprint) before being trusted, OR by a one-time operator confirmation the first time a `lan` candidate is seen for a given device. Without one of these, a compromised sibling can false-route dials. (See F6.)**

3. **Relay pipe lifetime policy: per-connection grants vs a standing pipe per peer-pair — is reconnect-per-idle-teardown churn acceptable at keepalive cadence?**
   **RECOMMENDATION: standing pipe per peer-pair, with idle-teardown on the standing pipe; the grant is the means to OPEN the pipe, and the pipe itself is long-lived. Reconnect-per-idle-teardown is operationally fragile (the fed-module must re-handshake Noise IK + re-establish the carrier on every idle cycle; this churns the AccountDO's WS state and risks reaping false-positives — see F14). Standing pipes with idle teardown AND a grace period on the reaper (F14) is the right shape. The pipe_token's TTL governs the OPEN, not the LIFETIME; once consumed, the pipe lives until idle-teardown or explicit close.**

4. **Should `connect_offer` require the target to be verified before signaling is relayed (quieter unpaired devices) or is offer-to-unverified needed for the pairing UX?**
   **RECOMMENDATION: offer-to-unverified is REQUIRED for the pairing UX, but the target's UX must be HARD-LOCKED to a code-compare display — not an Accept/Deny. Specifically: the target sees a connect_offer from an unverified pubkey, and the CK app renders the SHA-256 safety number derived from BOTH static keys. The single accept gesture is "Codes match." There is no Accept/Deny; the only way to proceed is to compare the codes. If the codes don't match, there is no "Deny" — there is "Codes don't match" with a single "Cancel" button. (See F2.)**

5. **WS hibernation vs signaling latency — acceptable to pay a cold-wake round-trip on first signal to an idle account?**
   **RECOMMENDATION: yes, but tune the signaling timeouts to absorb the cold-wake (T1=connect_request→connect_offer=5s, T2=connect_offer→connect_accept=10s; document the numbers), and probe-and-pin: keep the WS warm on any control-plane activity, only hibernate after N minutes of zero activity. Do not let the 2s per-candidate timeout in 5.4 stand as-is — it will routinely be blown by Cloudflare DO cold-wake, which is tens to hundreds of ms in the best case and seconds in the worst case. (See F10.)**

---

## SUMMARY

**Findings by severity:**
- BLOCKER: 4 (F1 effect-ledger × relay-pipe interaction; F2 unverified-offer UX; F3 pipe-token model underspecified; F4 rendezvous-down fallback unengineered)
- HIGH: 4 (F5 re-enroll + tombstone chain missing; F6 candidate self-reporting as sibling-attack surface; F7 device-token revocation race; F8 signaling abuse — no nonce/timestamp/rate-limit/signing)
- MEDIUM: 8 (F9, F10, F11, F12, F13, F14, F15, F16)
- LOW: 6 (F17, F18, F19, F20, F21, F22)

**Overall risk assessment: HIGH.** The architectural shape is sound and the inheritance from phase 0-2 is mostly honored. But four BLOCKERs each independently prevent a safe ship:
- A claim about "Noise session resumption = cheap" that does not survive inheritance check (F1).
- A claim about "rogue device never becomes routable" that depends on an unspecified UX rendering (F2).
- A claim about "single-use, short TTL" pipe tokens with no specification of who can consume whose grant (F3).
- A claim about "static profiles remain the availability fallback" with no engineering of the rendezvous-down code path (F4).

**Inheritance claims that did NOT survive the check against phase 2 v4.1:**
- F1: "Noise session resumption = ordinary re-handshake, cheap" — phase 2 says nothing about session resumption semantics or cost.
- F1/F14: "relay-path partitions classify exactly like TCP partitions" — phase 2's partition classifier is per-peer, not per-carrier; the equivalence is asserted, not inherited.
- F4: "Absent → phase-2 behavior exactly" — phase 2 has no rendezvous path; "exactly" requires engineering the absent path, which the doc doesn't do.
- F17: "3× reap window unchanged" — the phase 2 doc does not state a "3× reap window" number; this is internal fed-module state being re-asserted as a doc-level spec.

**Inheritance claims that DID survive the check:**
- Noise IK E2E (F1 — phase 2 2.2, 5.3).
- Verify-code ceremony gated first contact (F2 — phase 2 5.3 line 172).
- Exactly-once effect ledger (F1/F15 — phase 2 6.1).
- Per-peer keepalive + reaper partition classifier (F14 — phase 2 6.2).
- Reserved + nonce-bound fed-module (not directly challenged in this review).
- `federation_exposure` default-deny (F18 — phase 2 2.3).

**Overall verdict: NO-GO.**

Must-fix-before-build blockers (cannot ship without these):
1. **F1** — Spec the logical-peer / physical-pipe split: relay teardown is a transport event, not a partition event. Restate the effect-ledger's recovery contract across pipe reconnects. Drop the "cheap" claim or specify a real resumption mechanism.
2. **F2** — Lock the verify-code UX contract: a code-compare display with a single "Codes match" gesture; no Accept/Deny for unverified offers. Add a server-side enforcement that the offerer and target compute the code independently.
3. **F3** — Spec `pipe_token` as `HMAC(pipe_id_key, pipe_id || device_pubkey || exp || nonce)`; bind to device pubkey; require the WS's authenticated device identity to match. On idle teardown, mint a new `pipe_id` AND new tokens.
4. **F4** — Spec a rendezvous-dead timer and the static-profile fallback path explicitly. State the timer. State the signal that triggers it. State the test that proves the fallback works.

If those four are fixed, the design is ready for build with the HIGHs as a follow-up gate.