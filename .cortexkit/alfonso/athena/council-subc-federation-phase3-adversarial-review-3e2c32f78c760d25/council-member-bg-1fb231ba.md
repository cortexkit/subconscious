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