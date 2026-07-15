## F-01 — Enrollment has no proof-of-possession or uniqueness contract
- **Severity**: HIGH
- **Exact section**: Phase 3 4.2; compare Phase 2 5.3
- **Confidence**: High
- **The problem**: Phase 3 enrollment is just provider JWT + `{device_pubkey, device_name, platform}`, and it explicitly says enrollment “binds pubkey only.” That drops a load-bearing Phase 2 property: the cloud should not be able to enroll a device unless the device proves possession of a device-held secret. As written, a stolen provider account or compromised Worker can enroll an arbitrary pubkey, or race a duplicate enrollment for an existing pubkey. The doc also says nothing about unique `(provider, subject) -> account_id` resolution or unique `(account_id, device_pubkey)` enrollment under concurrency.
- **Concrete fix**: Make `/v1/device/enroll` a challenge-response flow signed by the device private key; enforce transactional unique constraints on provider-subject and device-pubkey rows; make enroll idempotent; require explicit signed recovery for re-enrolling a removed pubkey.
- **Inheritance check**: **Not supported.** Phase 2 5.3 explicitly requires a device-held secret at enrollment; Phase 3 removes that guarantee.

## F-02 — Device removal is only a best-effort delta, not a durable revocation
- **Severity**: BLOCKER
- **Exact section**: Phase 3 4.2, 5.3, 5.5; compare Phase 2 3.3, 6.2, 6.4
- **Confidence**: High
- **The problem**: Phase 3 says removal is: delete record, revoke token, broadcast `registry_delta`, and peers flip the profile entry to `revoked_by_account`. That is not enough. A previously verified device is trusted by pinned key, not by account membership. If the delta is dropped, delayed, replayed, or maliciously suppressed, another device can retain a verified peer entry and keep routing to the removed device on its old key/cached candidate. This is exactly the class of revocation Phase 2 solved with signed generations/tombstones.
- **Concrete fix**: Model removal as a durable signed revocation tombstone with monotonic generation; persist it locally before admitting future sessions; require every new session/profile refresh to consult the tombstone set; make re-enrollment of the same pubkey a distinct signed rotation/re-pair path.
- **Inheritance check**: **Phase 2 supports the stronger property.** Phase 2 6.4 requires signed revocation/tombstones, and 6.2 requires signed generations/tombstones. Phase 3 omits them.

## F-03 — Token revocation/rotation races are unspecified for live control sessions
- **Severity**: HIGH
- **Exact section**: Phase 3 4.2, 5.1, 5.3
- **Confidence**: High
- **The problem**: The device token is long-lived bearer auth, sent in `hello`, and AccountDO keeps a live WS per device. The doc never says revoke/rotate closes existing sessions, invalidates outstanding relay grants, or version-checks subsequent messages. A stolen token can therefore stay “online” and keep signaling after nominal revocation if it already has a control socket.
- **Concrete fix**: Give each token a `token_id`/version/expiry; bind each control WS to that version; on revoke/rotate, close all sessions for that device immediately and invalidate unconsumed grants derived from the old version.

## F-04 — Unverified signaling + self-reported LAN candidates creates an SSRF/scanning channel
- **Severity**: HIGH
- **Exact section**: Phase 3 4.3, 5.2, 5.3, 5.4,  Q2/Q4
- **Confidence**: High
- **The problem**: Same-account devices can signal each other before verification, and `lan` candidates are self-reported. That means a rogue sibling device, stolen account, or compromised cloud can inject `connect_offer`/`connect_accept` flows that cause a victim to dial attacker-chosen LAN/local addresses before any verify-code ceremony. The doc’s current answer to Q2 (“it can only redirect its own inbound dials”) is wrong; this is outbound network activity from the victim.
- **Concrete fix**: Do not allow ambient `connect_offer` to unverified peers. Pairing should require an explicit one-shot local invite capability. Also sanitize candidates: never dial loopback, link-local, multicast, or metadata ranges; for unverified peers, prefer public+relay only; only enable LAN dialing after explicit proof of same-network plausibility.
- **Inheritance check**: **Phase 2 does not justify this window.** Phase 2 supports “non-routable until code comparison” for tool trust, not pre-verification outbound dialing.

## F-05 — The dial/signaling state machine is contradictory and has no winner-election
- **Severity**: BLOCKER
- **Exact section**: Phase 3 5.3, 5.4; compare Phase 2 2.5
- **Confidence**: High
- **The problem**: 5.3 says both sides race candidates; 5.4 says the initiator dials by a “pubkey tie-break as phase 2.” I found no such tie-break rule in the Phase 2 design doc. Worse, signaling messages carry no request id / nonce / sequence, so delayed or replayed `connect_offer`/`connect_accept` messages are not bound to one attempt. If TCP and relay both authenticate near-simultaneously, “first-success-wins” is underspecified, while Phase 2 requires one active `(peer, remote module)` export path.
- **Concrete fix**: Specify the initiator rule in Phase 3 itself; add per-attempt `request_id` plus monotonic signaling sequence numbers; after authenticated session establishment, run deterministic winner election and tear the loser down before catalog exchange/HELLO.
- **Inheritance check**: **Unsupported as written.** The claimed Phase 2 tie-break is not in the Phase 2 doc; Phase 2 does support one-connection-per-peer/module, which makes winner-election mandatory.

## F-06 — Relay grant semantics are under-specified and Phase 2 DoS guardrails regress
- **Severity**: HIGH
- **Exact section**: Phase 3 5.3,  compare Phase 2 5.5
- **Confidence**: High
- **The problem**: `relay_grant` is ambiguous: 5.3 shows one `pipe_token` “issued to both sides,” while  says per-side tokens. The doc never says what a grant is bound to, where single-use is recorded, or how replay/second-use is rejected. It also drops Phase 2’s explicit quota/rate-limit requirement. Combined with one AccountDO per account, a noisy or stolen device can flood `connect_request`/`relay_open` and starve the whole account.
- **Concrete fix**: Make grants per-side and bind them to `(account_id, initiator_pubkey, responder_pubkey, side, token_version, expires_at, consumed_at)`; reject mismatched side/pubkey; require both control-plane identity and matching grant; enforce per-account/per-device quotas, outstanding-pipe caps, and rate limits before allocation.
- **Inheritance check**: **Phase 2 is stricter.** It explicitly requires auth-before-resource and per-account/per-device quotas/rate limits.

## F-07 — Rendezvous-down fallback is not airtight
- **Severity**: HIGH
- **Exact section**: Phase 3 5.4, 5.5, 
- **Confidence**: Medium-High
- **The problem**: The doc only guarantees exact Phase 2 behavior when `[rendezvous]` is absent. If `[rendezvous]` is configured but the control plane is down, connection initiation still appears to depend on `connect_offer/accept`, and static `addr` is described only as another candidate. That leaves a hole where a peer that is statically reachable in Phase 2 can become unavailable or hang because rendezvous is present-but-down.
- **Concrete fix**: Normatively require that pinned static peers remain directly dialable with zero rendezvous dependency. When the control WS is unavailable, the system must immediately degrade to static-candidate-only behavior and must not gate static dialing on registry `online` state or snapshot freshness.

## F-08 — “Relay partitions classify exactly like TCP” is unproven
- **Severity**: HIGH
- **Exact section**: Phase 3   compare Phase 2 6.2
- **Confidence**: Medium-High
- **The problem**: Phase 3 claims reaping is unchanged and relay-path partitions classify exactly like TCP, but RelayDO also has idle teardown and fresh-grant reconnects. If idle timeout is below the effective keepalive/reap envelope, healthy sessions become false partitions and generate spurious GOODBYEs / `OutcomeUnknown`. If keepalives keep the relay pipe busy, then the design is really a standing pipe, not reconnect-on-idle.
- **Concrete fix**: Specify timeout ordering now: keepalives must traverse the relay; `relay_idle` must be greater than the full reaper window or disabled for live sessions; every reconnect must be treated as a fresh session that reuses Phase 2 recovery reconciliation, not as magical “resumption.”
- **Inheritance check**: **Not supported as stated.** Phase 2 defines the reaper once a carrier drop is surfaced; it does not prove a stateful relay with idle teardown is equivalent to raw TCP.

## F-09 — WsCarrier framing mismatch behavior is undefined
- **Severity**: MEDIUM
- **Exact section**: Phase 3 
- **Confidence**: High
- **The problem**: Keeping the 4-byte length prefix inside WS message framing is only safe if the receiver enforces exact one-record-per-message semantics. The doc does not say what happens if a WS message contains multiple records, a partial record, or a length prefix that disagrees with message length.
- **Concrete fix**: State a hard rule: exactly one fed record per WS message; prefix must equal payload length; any mismatch is fatal before record parse.

## ANSWERS TO THE FIVE OPEN QUESTIONS

1. **Device-token custody on disk**
   - **Recommendation**: **Put it in the credentials vault immediately.**
   - **Why**: It is a long-lived bearer that grants control-plane impersonation. 0600 next to the device key is not enough for v1 if a vault already exists. At minimum, use vault/OS-keystore storage and rotate on export/import. All expiry/TTL decisions must use server time, not device wall clock.

2. **Self-reported registry candidates**
   - **Recommendation**: **Yes, harden it.**
   - **Concrete rule**: For unverified peers, do not use self-reported LAN at all. For verified peers, reject loopback/link-local/multicast/metadata ranges, and only try LAN when there is a same-network plausibility signal. Public candidate should remain server-observed; relay is the safe fallback.

3. **Relay pipe lifetime**
   - **Recommendation**: **Use a standing pipe per authenticated peer session, not reconnect-per-idle-blip.**
   - **Concrete rule**: One relay pipe per live Noise session / peer-pair. Keep it open while the session is alive; meter it; close only after long inactivity above the reap envelope or on true session death. Every real reconnect is a full fresh session using Phase 2 recovery rules.

4. **Should `connect_offer` require verification?**
   - **Recommendation**: **Routine offers yes; pairing no ambient offers.**
   - **Concrete rule**: Verified peers can signal normally. Unverified peers may signal only under an explicit, one-shot local pairing invite capability generated by user action on an already trusted device. Do not allow account-wide ambient offers to unverified devices.

5. **WS hibernation vs signaling latency**
   - **Recommendation**: **Acceptable only with strict guardrails.**
   - **Concrete rule**: Cold-wake latency must not count against the ~2s per-candidate data-path timers. Give signaling its own larger deadline budget, measure p95/p99 wake latency in acceptance tests, and keep static direct dialing available even when the AccountDO is cold or unreachable.

## OVERALL VERDICT

**NO-GO**

**Must-fix before build**
1. **F-02** — revocation/removal must be a durable signed tombstone/generation mechanism, not a best-effort delta.
2. **F-05** — the dial/signaling state machine needs explicit request identity, replay resistance, and single-winner arbitration.
3. **F-04** is not formally marked BLOCKER above, but I would not ship ambient offer-to-unverified + self-reported LAN dialing in a hostile-cloud threat model.

**Count**: 2 BLOCKER, 6 HIGH, 1 MEDIUM.

**Bottom line**: The crypto story is still mostly intact, but the control-plane lifecycle is under-specified in the exact places attackers and races live: enrollment proof, revocation durability, pre-verification signaling, dial arbitration, and relay grant binding. Those are Phase-3 design holes, not implementation nits.