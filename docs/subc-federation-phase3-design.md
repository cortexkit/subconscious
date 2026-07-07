# subc Federation Phase 3 — Cloud Rendezvous, Pairing, and Relay

Status: DRAFT v1 (council gate pending)
Owner: Alfonso @ subconscious. Builder: FED (cortexkit/subc-federation).
Predecessors: `docs/subc-federation-design.md` (v4.1, phases 0-2 shipped), fed-wire r4.1.

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
live profile reload.

## 2. What phase 2 already guarantees (load-bearing inheritance)

- **E2E Noise IK between device static keys.** Every fed byte between two
  devices is ciphertext under keys the cloud never sees. Any phase-3 cloud
  component can only move ciphertext or metadata; MITM requires defeating the
  key-pinning ceremony, not the infrastructure. This was deliberate in v2
  (council-forced) precisely to make a future relay untrusted-by-construction.
- **Trust = pinned keys + verify-code ceremony** (SHA-256 safety number,
  order-independent, fed-cli `verify-code --confirm`). Phase 3 does not touch
  this; it only automates *address discovery* and *transport reachability*.
- **Exposure = default-deny allowlist per peer**, verified-gate enforced at
  the accessor level both directions.
- **Exactly-once mutating semantics** (effect ledger) are transport-agnostic:
  they key on effect_id, not on how bytes traveled.

## 3. Architecture overview

```
 device A (laptop)                    CortexKit cloud (Cloudflare)                device B (home desktop)
 ┌───────────────┐                   ┌──────────────────────────────┐            ┌───────────────┐
 │ subc-core     │                   │ Worker (API, JWKS verify)    │            │ subc-core     │
 │  └ fed-module ├── WS (control) ──▶│  └ AccountDO (registry +     │◀── WS ─────┤ fed-module    │
 │               │                   │      signaling)              │            │               │
 │               │                   │  └ RelayDO (ciphertext pipe) │            │               │
 └──────┬────────┘                   └──────────────────────────────┘            └──────┬────────┘
        │                                                                                │
        └──────────── data path: direct TCP (LAN / public) ─────────────────────────────┘
                      or Noise-over-WebSocket via RelayDO when both sides are NATed
```

Three cloud pieces, all small:

1. **Account** — identity + entitlement root. Managed auth provider at the
   edge (v1: WorkOS AuthKit), our own account IDs and device credentials
   behind an `AccountVerifier` seam.
2. **Rendezvous** — per-account device registry + signaling channel
   (Cloudflare Worker + one Durable Object per account).
3. **Relay** — dumb ciphertext forwarder for the double-NAT case (Durable
   Object bridging two WebSockets).

The fed-module gains a **control-plane client** (one outbound WebSocket to the
rendezvous) and a **transport adapter** (Noise-over-WebSocket) — the Noise
session, fed-frame codec, forwarder, ledger, and profile model are unchanged.

## 4. Account (3a)

### 4.1 Provider at the edge, never the identity root

- The managed provider proves "this human owns this account" **once per device
  login**. Everything durable is ours: our `account_id` (ULID, minted at first
  login, keyed by provider subject), our device records, our tokens.
- v1 provider: **WorkOS AuthKit** — first-class OAuth 2.0 Device Authorization
  Flow (the axis that matters for a CLI + native-app product: login is plain
  HTTP + "enter code at url" from any client, zero provider SDK in our
  binaries), JWKS-published JWTs verifiable in the Worker, GitHub social +
  emailed-code auth, free tier ample. Swappable behind the seam.
- `AccountVerifier` seam in the Worker: `verify(provider_token) ->
  {provider, subject, email}`. Implementations: `WorkOsVerifier` (JWKS), plus
  a `FakeVerifier` for tests. Nothing else in the system sees provider tokens.

### 4.2 Device enrollment

1. `fed-cli login` (later: CK app) runs the device-authorization flow →
   provider JWT.
2. CLI calls `POST /v1/device/enroll` with the JWT + `{device_pubkey,
   device_name, platform}`.
3. Worker verifies via JWKS (`AccountVerifier`), resolves/creates
   `account_id`, and hands enrollment to that account's **AccountDO**, which
   records the device and mints a **device token**: an opaque bearer bound to
   `(account_id, device_pubkey)`, stored hashed in the DO, revocable,
   long-lived with rotation. Day-to-day rendezvous auth uses this token;
   the provider is out of the loop until the operator re-logins.
4. The private key never leaves the device. Enrollment binds pubkey only.

Device removal: `fed-cli device remove <name-or-fp>` or CK app → DO deletes
the record + revokes the token + broadcasts a registry delta; peers unpair it
(profile entry flips to `revoked_by_account`, non-routable, requires re-pair
if re-enrolled).

### 4.3 What "same account" grants (F4, locked)

**Discovery only.** Devices on one account see each other's registry entries
(name, pubkey fingerprint, candidate endpoints, last-seen, online bit) and can
open signaling to each other. **Routability still requires the phase-2
verify-code ceremony** — the CK app will render this as compare-and-tap;
fed-cli renders it as today. A cloud/account compromise therefore yields
metadata + the ability to *offer* a rogue device for pairing — it cannot make
any existing device trust the rogue one, and the rogue device's verify-code
will not match anything the operator's real devices display. Device-join
events are pushed loudly to all enrolled devices.

## 5. Rendezvous (3a)

### 5.1 Shape

- **Worker**: stateless HTTP + WebSocket upgrade endpoint; JWKS verification
  on enroll; device-token check on everything else; routes to the account's DO
  by `account_id`.
- **AccountDO** (one per account): SQLite-backed device registry; holds the
  live WebSocket per online device (hibernation-friendly — WS hibernation
  keeps idle accounts at ~zero cost); pushes registry deltas and signaling
  messages.

### 5.2 Registry entry

```jsonc
{
  "device_pubkey": "<32B hex>",
  "name": "ufuk-mbp",
  "platform": "darwin-arm64",
  "candidates": [                 // self-reported, refreshed on connect + change
    {"kind": "lan",    "addr": "192.168.1.34:7841"},
    {"kind": "public", "addr": "83.46.225.175:7841"},   // server-observed, STUN-free
    {"kind": "relay"}                                    // always available
  ],
  "last_seen_ms": 0,
  "online": true
}
```

The `public` candidate is stamped **server-side** by the Worker from the
connection's observed source IP (+ the device's configured listen port).
No STUN dependency; a NATed device whose observed ip:port is not actually
dial-able simply fails that candidate and falls to relay.

### 5.3 Signaling ops (over the control WS, JSON, versioned `rdv-v1`)

- `hello {device_token}` → `registry_snapshot`
- `registry_delta` (push: device added/removed/online/candidates-changed)
- `connect_request {to: pubkey}` → relayed to the target as
  `connect_offer {from: pubkey, candidates}` → target answers
  `connect_accept {candidates}` — both sides then race candidates (§6).
- `relay_open {to: pubkey}` → `relay_grant {relay_url, pipe_id, pipe_token}`
  issued to both sides (§7).

Signaling carries **no secrets and no trust decisions**: candidates and
pubkeys only. A malicious rendezvous can deny service or hand out wrong
addresses; it cannot impersonate (Noise IK fails against the pinned key) and
cannot read tool traffic.

### 5.4 Dial policy (F1(b) resolved: LAN needs no punching)

On `connect_offer/accept`, the initiator (pubkey tie-break as phase 2) tries
candidates in order: **lan → public → relay**, with a short per-candidate
timeout (~2s) and first-success-wins. Same-LAN devices connect directly with
the cloud only having brokered addresses; laptop+VPS connects via the public
candidate exactly as phase 2; only cross-network double-NAT lands on relay.
The winning transport is remembered per peer and retried first next time,
with periodic re-probes for direct upgrade while on relay.

### 5.5 Fed-module integration

- New `[rendezvous]` profile section: `{account: true, control_url,
  device_token_path}`. Absent → phase-2 behavior exactly (static profiles
  keep working forever; the WAN test rig never needs an account).
- Registry-discovered peers materialize as **unverified profile peers**
  (name from registry, `verified:false`) — the existing enforced gate makes
  them non-routable until the ceremony; discovery adds candidates, not trust.
- Static `addr` in a profile acts as one more candidate (highest priority)
  for hybrid setups.

## 6. Transport adapter (3a/3b boundary)

`fed-transport` grows a second carrier under the same Noise session code:

- `TcpCarrier` — today's path, unchanged.
- `WsCarrier` — Noise handshake + fed-frames over WebSocket binary messages
  (one fed record per WS message; the 4-byte length prefix is redundant inside
  a message framing but kept identical so record parsing is carrier-agnostic
  and golden vectors hold). Used for the relay path; also trivially enables
  future browser-adjacent clients.

Everything above the carrier (Noise, strict JSON, effect ledger, forwarder,
keepalive/reaping) is carrier-blind. Keepalive cadence and 3× reap window
unchanged; relay-path partitions classify exactly like TCP partitions.

## 7. Relay (3b)

**RelayDO**: created per pipe grant; bridges exactly two authenticated
WebSockets (`pipe_id` + per-side `pipe_token`, single-use, short TTL);
forwards binary messages verbatim in both directions; enforces the 16 MiB
frame cap and per-pipe flow (WS backpressure propagates end-to-end); idle
timeout tears the pipe down (peers reconnect via a fresh grant — Noise session
resumption = ordinary re-handshake, cheap).

Properties:
- **Zero knowledge**: the relay sees Noise ciphertext between two pinned
  static keys. It learns traffic volume/timing and the account's device graph
  — the same metadata the rendezvous already holds.
- **No open inbound**: both devices dial OUT to the relay; no port forwarding,
  no firewall changes, works under CGNAT.
- Egress economics: Workers/DO WS transfer is viable at tool-RPC volumes;
  relay minutes/bytes are the natural paid-tier meter later (out of scope for
  the phase gate).

## 8. Threat model deltas (vs phase 2)

| Actor | Gains | Blocked by |
|---|---|---|
| Compromised cloud (Worker/DO) | metadata (device graph, online times, volumes), DoS, wrong addresses, rogue pairing *offers* | Noise IK against pinned keys; verify-code ceremony; loud device-join events |
| Stolen provider account (WorkOS login) | enroll a rogue device → it appears in the registry | Ceremony: rogue device never becomes routable without operator confirming codes on an existing device; join events loud |
| Stolen device token | impersonate that device to the rendezvous (signaling + registry) | Cannot speak Noise as the device (no private key); revocable server-side |
| On-path network attacker | same as phase 2 | Noise (and WS carrier runs over TLS to the relay besides) |

Explicitly accepted: rendezvous/relay are availability dependencies for the
seamless path (static profiles remain the availability fallback); metadata
visibility at the cloud (standard for this class of product; documented).

## 9. Build plan (one phase gate, sequential)

- **3a-1** `fed-rendezvous` (new worker package in cortexkit/subc-federation:
  TS, wrangler, AccountDO): enroll + registry + signaling + server-observed
  public candidates + device tokens. `FakeVerifier` first; WorkOS wired
  behind the seam second. Miniflare/workerd tests.
- **3a-2** fed-module control client + candidate dialer + registry-to-profile
  materialization + `fed-cli login/devices`. E2E: two local daemons + real
  workerd rendezvous, LAN candidate wins, ceremony over discovered peer.
- **3a-3** WorkOS live: device-flow login end-to-end on the dev account;
  enrollment on laptop + Hetzner box; real cross-machine pair via rendezvous
  (public candidate path).
- **3b-1** `WsCarrier` + golden-vector parity proof (same records both
  carriers).
- **3b-2** RelayDO + pipe grants; E2E with both daemons' direct candidates
  artificially blocked (iptables) → relay connect → tool call → exactly-once
  drill over relay; then a real double-NAT drill (laptop on phone hotspot ↔
  home network).
- Gate: council review of this doc first; FED builds; phase gate =
  3a+3b acceptance runs above.

## 10. Open questions for the council

1. Device-token custody on disk (0600 file next to the device key today) —
   sufficient for v1, or fold into the credentials vault immediately?
2. Registry `candidates` self-reporting: any hardening needed against a
   malicious *sibling device* on the same account lying about its LAN addr
   (current answer: it can only redirect its own inbound dials; Noise pins
   identity regardless)?
3. Relay pipe lifetime policy: per-connection grants vs a standing pipe per
   peer-pair — is the reconnect-per-idle-teardown churn acceptable at
   keepalive cadence?
4. Should `connect_offer` require the target to be *verified* before
   signaling is relayed (quieter unpaired devices) or is offer-to-unverified
   needed for the pairing UX itself? (Current design: allowed, loud.)
5. WS hibernation vs signaling latency: acceptable to pay a cold-wake
   round-trip on first signal to an idle account?
