# subc Federation Phase 3 — Cloud Rendezvous, Pairing, and Relay

Status: v2 (council findings folded; ready for FED scoping)
Owner: Alfonso @ subconscious. Builder: FED (cortexkit/subc-federation). Account service: CKCRED (see §4).
Predecessors: `docs/subc-federation-design.md` (v4.1, phases 0-2 shipped), fed-wire r4.1.
Council: `.cortexkit/alfonso/athena/council-subc-federation-phase3-adversarial-review-3e2c32f78c760d25/` — verdict GO-WITH-CHANGES; all seven blockers are folded below and marked [C#n].

## 1. Goal

Phase 2 delivered cross-machine tool federation for operators willing to
hand-manage endpoints: static IP/port profiles, fed-cli pairing, at least one
publicly reachable side. Phase 3 removes the hand-management: a user logs into
their CortexKit account on each device, devices discover each other, pair with
an explicit verification ceremony, and connect — including when both sides sit
behind NAT on different networks. This is the "single login makes subcs
connect" product shape, and the first paid-tier surface.

Non-goals (phase 4+): QUIC hole-punching (latency optimization of the relay
path, never a capability change), teams/multi-user trust, multi-hop routing,
live profile reload, signaling-DO sharding.

## 2. What phase 2 already guarantees (verified inheritance)

- **E2E Noise IK between device static keys.** Every fed byte between two
  devices is ciphertext under keys the cloud never sees. Any phase-3 cloud
  component can only move ciphertext or metadata; MITM requires defeating the
  key-pinning ceremony, not the infrastructure.
- **Trust = pinned keys + verify-code ceremony** (SHA-256 safety number,
  order-independent, fed-cli `verify-code --confirm`).
- **Exposure = default-deny allowlist per peer**, verified-gate enforced at
  the accessor level both directions.
- **Exactly-once mutating semantics** (effect ledger) key on effect_id
  `(origin_device_pubkey, incarnation_uuid, seq)` — pipe-agnostic and sound
  across reconnects. (What is NOT inherited: the reconciliation *trigger* and
  retention clock — see §7.3.)

Phase 3 introduces mechanisms phase 2 does not have. These are NEW and
specified as such (the v1 draft wrongly presented three of them as inherited):
the dial-initiator rule and multi-carrier session management (§5.4), and
transport-reconnect semantics over a relay (§7.2-§7.3). [C#2][C#3][C#4]

## 3. Architecture overview

```
 device A (laptop)                    CortexKit cloud (Cloudflare)                device B (home desktop)
 ┌───────────────┐                   ┌──────────────────────────────┐            ┌───────────────┐
 │ subc-core     │                   │ Account service (CKCRED)     │            │ subc-core     │
 │  └ fed-module ├── WS (control) ──▶│  login flows + our JWTs      │◀── WS ─────┤ fed-module    │
 │               │                   ├──────────────────────────────┤            │               │
 │               │                   │ Rendezvous Worker (FED)      │            │               │
 │               │                   │  └ AccountDO (registry +     │            │               │
 │               │                   │      signaling)              │            │               │
 │               │                   │  └ RelayDO (ciphertext pipe) │            │               │
 └──────┬────────┘                   └──────────────────────────────┘            └──────┬────────┘
        │                                                                                │
        └──────────── data path: direct TCP (LAN / public) ─────────────────────────────┘
                      or Noise-over-WebSocket via RelayDO when both sides are NATed
```

Cloud pieces, all small, split by ownership:

1. **Account service** (CKCRED-owned) — the human-identity root: login flows
   (GitHub, Google, Apple, email magic link), our `account_id`s, our JWTs +
   JWKS. Ownership boundary: CKCRED owns everything up to "a valid CortexKit
   JWT names this account_id"; everything device-shaped — `/v1/device/enroll`,
   device tokens, the device registry, the account signing key, deltas and
   tombstones — is FED's rendezvous, which verifies the CKCRED JWT via JWKS
   and owns the rest.
2. **Rendezvous** (FED-owned) — per-account device registry + signaling
   (Worker + one Durable Object per account).
3. **Relay** (FED-owned) — dumb ciphertext forwarder for the double-NAT case.

The fed-module gains a control-plane client (one outbound WebSocket to the
rendezvous) and a WebSocket transport carrier; the Noise session, fed-frame
codec, forwarder, ledger, and profile model are unchanged.

## 4. Account (3a) — CKCRED-owned service, no third-party provider

Decision (Ufuk, 2026-07-07): roll our own. The end-user base is not
developers-only, so GitHub-only is not enough, and a managed provider
(WorkOS/Clerk class) buys us only the front door while adding a vendor on the
identity path. All four methods ship day 1:

| Method | CLI/native flow | Notes |
|---|---|---|
| GitHub | native OAuth device flow (RFC 8628) | zero scopes; identity only |
| Google | native device flow | identity/email scopes only |
| Apple | web OIDC + hosted callback page displaying a paste-code | Apple has NO device flow; our Worker hosts the redirect target; fully native later in the CK app. Safety constraints on the hosted page: it displays a one-time short-TTL paste code bound to the initiating CLI's nonce — never a provider token or CK JWT in the URL, page, or logs; the redirect target is the fixed first-party domain (no open redirect); the page shows the logging-in account identity as anti-phishing copy |
| Email magic link / code | Cloudflare Email Service + code storage in the account DO + rate limits | |

- **This is CortexKit's account primitive**, not a fed-only login: one
  `account_id` (ULID) with provider subjects mapped onto it, entitlements
  attached later. It lives in its own CKCRED-owned repo/worker.
- The account service issues **our JWTs** and publishes **our JWKS**. The
  rendezvous's `AccountVerifier` seam verifies those (token in → account_id
  out) and knows nothing about login methods. Provider tokens never reach the
  rendezvous.
- Account resolution `(provider, subject) → account_id` is **atomic**:
  serialized through a DO keyed by `hash(provider_subject)` (or a D1 unique
  index), so concurrent first-logins cannot split one human into two
  accounts. [C#6]

### 4.1 Device enrollment (proof-of-possession, atomic) [C#6]

1. `fed-cli login` (later: CK app) runs a login flow → account JWT.
2. CLI calls `POST /v1/device/enroll` with the JWT + `{device_pubkey,
   device_name, platform}`.
3. The AccountDO issues a **challenge nonce** over the enrollment metadata;
   the CLI signs it with the device's Noise static private key; the DO
   verifies against `device_pubkey` before recording anything. The cloud (or
   a stolen JWT) cannot enroll a pubkey it does not control.
4. `(account_id, device_pubkey)` is unique, enforced by atomic
   upsert-or-reject inside the DO. Re-enrolling an already-enrolled pubkey is
   rejected unless it carries a rotation proof (old key signs the new
   enrollment — the phase-2 rotation ceremony). Re-enrolling a **tombstoned**
   pubkey is allowed but lands as first-contact (§4.3).
5. The DO mints a **device token**: names `(account_id, device_pubkey,
   token_id, expiry)`; it is **proof-of-possession, not bearer** — every
   control-WS auth (`hello`) and enroll-adjacent call carries a fresh
   DO-issued challenge signed by the device static key, and the token merely
   names which device is proving. File theft without the private key yields
   nothing. Custody: 0600 next to the device key; vault integration is a
   fast-follow, not a v1 gate. [C-Q1][#10]
6. On revoke/rotate: the old token is atomically invalidated, all of that
   device's live control-WS sessions AND live relay pipes are closed
   immediately, and unconsumed relay grants derived from it are invalidated
   (no two-valid-token window). The RelayDO validates token version not only
   at WS upgrade but on an ongoing basis (per heartbeat interval), so a
   revoked device cannot keep an already-open pipe alive. [#10]

### 4.2 What "same account" grants (locked)

**Discovery only.** Devices on one account see each other's registry entries
and can open signaling. **Routability requires the verify-code ceremony.**

**Verification state is local-authored only.** [C#8] The `peer_pubkey →
verified` mapping is written only by the device itself, after a local
ceremony, into the device's own profile store. Anything cloud-delivered
(registry entries, connect offers, candidate profiles) materializes as
`verified:false` and the fed-module MUST ignore any `verified:true` arriving
from the cloud path. Phase-2 §3.3's "cloud-distributed profile is the single
source of truth" is hereby narrowed: it may distribute peers and candidates,
never verification state. A compromised cloud can therefore offer rogue
devices and lie about addresses; it can never make a device routable.

### 4.3 Device removal = durable signed revocation tombstone [C#7]

Removal is not a best-effort delta. The AccountDO persists a **revocation
tombstone** `{device_pubkey, enrollment_id, generation}` signed by the
**account signing key** — a keypair minted per account in the DO whose public
half every device pins at enrollment (the same key signs `registry_delta`
and `connect_offer`, closing the fake-event hole [#17]). On receipt each
device: persists the tombstone locally (survives restarts and outlives
registry state), closes live Noise sessions to that pubkey, invalidates
outstanding relay grants, refuses future handshakes, and honors the tombstone
over any static `addr` in its profile. Offline devices receive the tombstone
backlog on their next `hello` (retained server-side ≥ 30 days).

A tombstoned pubkey that re-enrolls is **first contact**: non-routable,
full code-compare ceremony, with the UX explicitly labeling it "previously
removed device re-enrolling" — never the quiet re-trust path, even though the
verify-code would match (same key, possibly attacker-held).

## 5. Rendezvous (3a)

### 5.1 Shape

- **Worker**: stateless HTTP + WS upgrade; verifies account JWTs via the
  account service's JWKS; routes to the account's DO.
- **AccountDO** (one per account): SQLite-backed device registry; live WS per
  online device (hibernation-friendly); holds the account signing key; pushes
  signed registry deltas and signaling messages. v1 bounds: device cap 50 per
  account; per-device token-bucket rate limits on every signaling op (e.g.
  10/min per source, 30/min per target); documented single-DO throughput
  ceiling — sharding is phase 4+. [#12]

### 5.2 Registry entry

```jsonc
{
  "device_pubkey": "<32B hex>",
  "name": "ufuk-mbp",
  "platform": "darwin-arm64",
  "candidates": [
    {"kind": "lan",    "addr": "192.168.1.34:7841", "generation": 12, "observed_at_ms": 0, "expires_at_ms": 0},
    {"kind": "public", "addr": "83.46.225.175:7841", "generation": 12, "observed_at_ms": 0, "expires_at_ms": 0},
    {"kind": "relay"}
  ],
  "last_seen_ms": 0,   // server-stamped
  "online": true
}
```

- The `public` candidate is **always server-stamped** from the connection's
  observed source IP + the device's configured listen port; a device-supplied
  `public` value is overwritten, never trusted. [#9]
- Candidates carry server-issued `generation` / `observed_at` / `expires_at`;
  a device's WS writes may only mutate its **own** row, and candidate updates
  are coalesced/rate-limited by the DO (a flapping device cannot churn its
  peers' dial state faster than the coalescing window). [#15]
- All security-relevant times (`last_seen_ms`, token/grant expiries, candidate
  freshness) are **server-authoritative** (Cloudflare wall-clock); devices
  never make TTL/trust decisions on local wall-clock. The fed-module's reaper
  keeps using the device **monotonic** clock. [#16]

### 5.3 Signaling ops (control WS, JSON, versioned `rdv-v1`) [#5]

Every device→DO message carries a monotonic per-(device, WS-session) `seq`;
the DO rejects `seq ≤ last-seen`. DO→device messages that assert account
state (`registry_delta`, `connect_offer`, tombstones) are **signed by the
account signing key**. `relay_open`/`relay_grant` carry a short-window server
timestamp and a request nonce (a replayed `relay_open` mints a
duplicate-rejected grant).

- `hello {device_token, challenge_sig}` → `registry_snapshot` + tombstone/delta backlog
- `registry_delta` (signed; device added/removed/online/candidates-changed)
- `connect_request {to: pubkey, offer_nonce}` → relayed as signed
  `connect_offer {from, candidates, offer_nonce, snapshot_hash}` (offers and
  `device_added` join events share the tombstone backlog retention: offline
  devices receive signed join events on next `hello`, retained ≥ 30 days) →
  answered by
  `connect_accept {candidates, offer_nonce}`; accept candidates must be a
  subset of the answerer's current registry row (the DO stamps the snapshot
  hash). Stale/expired offers are ignored. [#15]
- `relay_open {to: pubkey, nonce}` → per-side `relay_grant` (§7.1).

Signaling carries no secrets and no trust decisions. Signaling has its **own
deadline budget (≥5s)**, decoupled from candidate dialing: the per-candidate
dial timer starts only after `connect_accept` arrives; the initiator's
`connect_request` also warms the target DO. Cold-wake p99 gets measured in
3a-1 and the budget tuned from data. [#13]

### 5.4 Dial policy — deterministic single initiator [C#2]

**Rule (refined during the E drill — reachability-aware):** dialing is
permitted by REACHABILITY, arbitrated by pubkey. Either side may dial a
peer's **public or LAN** candidate it has discovered (subject to the hygiene
rules below); the **lexicographically lower pubkey** device is the PREFERRED
initiator and the only side that initiates **relay** establishment (§7.1
pipes are paid resources; exactly one side may open them). Determinism is
enforced where it actually matters — at completion, not at dial time: the
single-session-per-peer arbitration below deterministically resolves any
double-completion, so simultaneous dials are safe by construction.

Rationale: the original sole-initiator form was reachability-blind. With a
NAT'd lower-pubkey device and a public higher-pubkey device, the only
physically available direct path (NAT'd side dials the public side) was
forbidden by pubkey order, deadlocking direct connection pre-relay and
pushing ~half of NAT+public pairs onto the relay for no physical reason.
The safety property C#2 actually needs is no-eviction-on-double-win, and
that is owned by the handshake-time arbitration, not by dial-time
self-restraint.

The §5.6 pairing window authorizes a device to RECEIVE and respond to
unverified offers; it never widens dial permission (a windowed device dials
only per the reachability rule above; the window only gates its willingness
to answer). On `connect_offer/accept` the dialer tries candidates in order
**lan → public** (relay only for the lower-pubkey side) with a short
per-candidate timeout (~2s, post-accept only), Noise-handshake failure
short-circuiting to the next candidate immediately.

**Single-session-per-peer invariant:** after a Noise handshake completes,
the sides exchange an authenticated `connection_attempt_id`; a second
completed handshake to an already-connected peer is refused with a typed
error and torn down by the joiner BEFORE any re-HELLO or re-export — a
double-win (e.g. TCP and relay-WS completing simultaneously) can therefore
never evict a live session or flap. Golden test: both carriers complete at
once; exactly one session survives, zero GOODBYEs on the survivor.

Candidate hygiene on the dialer [#9]: reject loopback, link-local,
multicast, and cloud-metadata ranges; accept `lan` candidates only within the
dialer's own observed private subnet; for **unverified** peers dial
public+relay only (no LAN) — closes the SSRF/port-scan hole where a malicious
sibling advertises internal addresses for the dialer to probe. The winning
transport is remembered per peer and retried first, with periodic re-probes
for direct upgrade while on relay.

### 5.5 Fed-module integration and rendezvous-down degradation [#14]

- New `[rendezvous]` profile section: `{account: true, control_url,
  device_token_path}`. Absent → phase-2 behavior exactly.
- Registry-discovered peers materialize as unverified profile peers
  (`verified:false` — always; §4.2). Discovery adds candidates, never trust.
- Discovered candidates are **persisted locally**, so a known peer survives a
  cloud outage and a restart.
- **Signaling-independent direct dial:** static and last-known candidates are
  dialed immediately at startup and on demand, regardless of control-WS
  state. The control-WS connect has a bounded timeout (2-5s), after which the
  module proceeds on persisted candidates and retries the rendezvous in the
  background. The module surfaces a `rendezvous: connected | unreachable |
  disabled` status so "registry empty" and "registry unreachable" are
  distinguishable. Acceptance test: kill the Worker mid-run → a
  static-reachable peer stays connected; a discovered-only peer fails with
  `rendezvous_unreachable`, no hang.

### 5.6 Pairing UX (offer-to-unverified) [C-Q4]

Offers to unverified devices are allowed (pairing needs a channel to compare
codes over) but are **user-initiated, rate-limited, and windowed**: the
target opts into a short-lived pairing window (`fed-cli pair --window`, CK
app button); outside it, unverified offers are dropped quietly (config:
`accept_unverified_offers`, default windowed). The accept UX is hard-locked
to **code-compare** — the only affirmative gesture is "codes match" after
both devices display the verify-code; never Accept/Deny. Device-join events
render as signed, un-dismissible banners showing the new device's fingerprint
until acknowledged. [#17]

## 6. Transport adapter

`fed-transport` grows a second carrier under the same Noise session code:

- `TcpCarrier` — today's path, unchanged.
- `WsCarrier` — Noise handshake + fed records over WS binary messages.
  **Framing invariant (normative):** exactly one fed record per WS message,
  and `ws_message_len == 4 + length_prefix_value` (the prefix is retained as
  a consistency check, not a length source). Any mismatch — zero records,
  concatenation, or prefix disagreement — closes the carrier with a typed
  framing error; receivers never cross-message-buffer or silently truncate.
  Golden negative vectors for each mismatch case. [#11]

Everything above the carrier (Noise, strict JSON, effect ledger, forwarder)
is carrier-agnostic **except partition classification and recovery, which are
extended for relay transport in §7.2-§7.3** (they are NOT "unchanged").

## 7. Relay (3b)

### 7.1 RelayDO and pipe grants [C#1]

A `relay_open` mints **per-side tokens**: each `pipe_token = HMAC(relay_key,
pipe_id ‖ device_pubkey ‖ side ‖ exp ‖ nonce)`, bound to that device and that
side of that pipe. The RelayDO atomically check-and-consumes each token
exactly once inside its single-threaded WS-upgrade handler (DO serialization
removes the TOCTOU), rejects redemption by the wrong device or for the wrong
side, and binds the accepted WS to the proven device identity (the PoP
challenge from §4.1 is presented at upgrade). Tokens are single-use and
short-TTL (server-clock). After any teardown, the old `pipe_id` is retired
and a reconnect mints a new `pipe_id` + fresh per-side tokens — a stolen or
replayed token can never occupy a slot of a live pipe or a future one.

Pipe properties: bridges exactly two authenticated WebSockets; forwards
binary messages verbatim; enforces the 16 MiB frame cap; WS backpressure
propagates end-to-end; the relay sees only Noise ciphertext + volume/timing
metadata.

### 7.2 Pipe lifetime: standing pipe + dormant state [C#3][C-Q3]

The pipe is **standing per active peer-pair**: kept warm while the fed
session is live (keepalives DO traverse it; warm relay time is the natural
paid-tier meter), with `relay_idle_timeout ≥ max(10× keepalive cadence,
5 min)` so teardown only happens when the fed session itself has gone idle
beyond the reap envelope.

**Dormant ≠ partitioned (NEW state, not phase-2 inherited):** on idle
teardown the RelayDO sends a distinct close code (`4000 idle`). The
fed-module then marks the peer **dormant**: routes and re-exports stay
registered, the partition reaper is suspended for a bounded
`reconnect_grace`, and a fresh-grant reconnect is attempted on demand or
next keepalive. Only a reconnect failure that exhausts the grace, or a true
keepalive miss that survives a successful reconnect, classifies as a
partition and fires the phase-2 reap (GOODBYE, re-export withdrawal). A
relay idle-teardown can therefore never masquerade as a peer partition or
strand an in-flight mutating effect's classification.

### 7.3 Recovery over reconnects [C#4]

Phase-2 recovery reconciliation is restart-triggered. Phase 3 extends the
trigger: on **every carrier reconnect** (relay re-grant or TCP re-dial), the
origin replays the §6.1 recovery query over the new transport for every
`sent`-without-outcome effect_id **before resuming new traffic**. A transport
reconnect does NOT mint a new incarnation (effect_id stays stable — the
ledger keying is already pipe-agnostic). Serving-ledger grace expiry is gated
on **elapsed reachable time** (time the origin was actually connected), not
wall-clock, so a long relay outage cannot expire an unconfirmed mutation
outcome into `effect_outcome_expired`. Re-handshake cost is measured in the
3b-2 drill and budgeted, not asserted.

## 8. Threat model deltas (vs phase 2)

| Actor | Gains | Blocked by |
|---|---|---|
| Compromised cloud (any Worker/DO) | metadata (device graph, online times, volumes), DoS, wrong addresses, rogue pairing offers | Noise IK against pinned keys; local-authored verification state [C#8]; signed deltas/tombstones; ceremony hard-locked to code-compare |
| Stolen account credential (login) | enroll a rogue device (visible, signed join banner) | enrollment PoP (can't enroll a pubkey it doesn't hold); ceremony gates routability |
| Stolen device-token file | nothing (PoP: token is inert without the device private key) | §4.1(5) |
| Malicious sibling device | candidate poisoning limited to its own row; offer spam within rate limits | dialer candidate hygiene [#9]; per-device rate limits; signed offers |
| On-path network attacker | same as phase 2 | Noise (+ TLS on the WS legs) |

Availability: rendezvous/relay are availability dependencies for the
*seamless* path only; static/persisted candidates keep paired peers reachable
through cloud outages (§5.5), and `[rendezvous]`-absent remains exact phase-2.

## 9. Build plan (one phase gate, sequential)

- **3a-0** (CKCRED, parallel): account service skeleton — GitHub device flow
  first, our JWT + JWKS; Google/Apple/email follow within the phase. FED
  consumes only the JWKS (JWT-in → account_id-out), so 3a-1 can start against
  a `FakeVerifier` immediately. `/v1/device/enroll` and everything
  device-shaped is FED's (§3 ownership boundary).
- **3a-1** (FED): rendezvous Worker + AccountDO — enroll (PoP challenge,
  atomic uniqueness), signed registry/deltas/tombstones, signaling (seq,
  nonces, rate limits), server-stamped candidates. Miniflare/workerd tests,
  incl. cold-wake p99 measurement.
- **3a-2** (FED): fed-module control client + single-initiator dialer +
  candidate hygiene + persisted candidates + rendezvous-down degradation test
  + `fed-cli login/devices/pair`.
- **3a-3**: live: enroll laptop + Hetzner box, pair via rendezvous, direct
  public-candidate path cross-machine.
- **3b-1** (FED): `WsCarrier` + framing negative vectors + golden parity.
- **3b-2** (FED): RelayDO + per-side grants + dormant state +
  reconnect-triggered reconciliation. Drill: direct candidates blocked by
  iptables → relay connect → exactly-once mutating drill over relay,
  including a forced idle-teardown mid-effect (must classify dormant, settle
  on reconnect, zero OutcomeUnknown); then a real double-NAT run (phone
  hotspot ↔ home network).
- Phase gate = the acceptance runs above + a re-gate council pass on this v2.

## 10. Resolved questions (council)

1. **Device-token custody**: PoP token + 0600 co-location; vault fast-follow.
2. **Candidate self-reporting**: hardened per §5.2/§5.4 (the v1 "only its own
   inbound" answer was wrong — the dialer was the victim).
3. **Pipe lifetime**: standing pipe + dormant state (§7.2).
4. **Offer-to-unverified**: allowed, windowed + user-initiated + rate-limited,
   code-compare-only UX (§5.6).
5. **Hibernation cold-wake**: acceptable with the decoupled ≥5s signaling
   budget (§5.3); measured in 3a-1.
