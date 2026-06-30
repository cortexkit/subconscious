# subc Federation & Profiles — Design Plan

**Status:** DRAFT for council gap-finding. Foundation decisions locked; several data-plane / identity forks open (see §8).
**Owner:** subc (Alfonso @ subconscious).
**Scope:** Cross-machine subc-to-subc federation, the profiles/identity layer, the optional cloud control plane, and the roaming-client (mobile/iPad) relay path. This is a future subsystem; nothing here is built yet.

---

## 1. Motivation & use cases

Users will run subc on more than one machine at once and want their tools and agents to feel like one fabric. Three concrete scenarios drive the design:

1. **Peer federation (laptop ↔ VPS).** A user runs subc on their personal computer and on a VPS simultaneously, with some agents running on the VPS and some on the computer. An agent running on the VPS must be able to call a tool registered on the personal computer (and vice versa), seamlessly. The CK app sets this up: install subc on both, connect them, automatic discovery of each other's catalogs, transparent cross-machine tool calls.

2. **Cloud single-login federation.** Instead of manual pairing, a single account login makes a user's subc instances discover and connect to each other automatically. This is the convenience tier (plausibly a paid feature) layered on top of the same federation mechanism.

3. **Roaming client reaches home (mobile / iPad).** An unsavvy user wants to reach their home subc from the CK app on their phone or iPad when they are away from home, easily and securely (no VPN setup, no port-forwarding). A small cloud endpoint lets the mobile app reach the home subc through a secure relay, so the user can drive or observe their home agents from anywhere.

The common thread: **make a user's tools and agents reachable across machines, securely, with zero manual networking, without weakening subc's same-host security model or thickening subc-core.**

---

## 2. Locked decisions

These are settled and are the foundation the rest of the design builds on.

### 2.1 Foundation (prior decision, carried forward)
Cross-machine topology is **Federation (A)**: every machine runs its **own** subc daemon plus its own local modules, **loopback-only**, key-on-disk, same-host transport threat model. All WAN traffic is **quarantined to a future subc-supervised InterSUBC federation module**, keeping modules network-unaware and preserving the same-host threat model. (Recorded in `docs/subc-core-architecture.md` §11 decision log and exercised as the `federation` archetype in `crates/subc-core/tests/closure.rs`.)

### 2.2 F1 — Tailscale-style control/data split
The cloud is a **control plane only**: identity, discovery, and policy distribution. **Tool-call bytes go peer-to-peer directly** between machines. The cloud relays data **only** as a NAT fallback (or for roaming clients with no direct path), and **never sees tool payloads** (end-to-end encryption through any relay). This is the Tailscale model: a coordination server for identity + the network map, WireGuard/DERP for the data plane, data direct-when-possible.

**Hard line:** a user's code and file contents never transit our servers in plaintext. The cloud forwards ciphertext at most.

### 2.3 F2 — default-deny per-peer capability scoping, keyed on `execution_mode`
Federation **inverts subc's trust boundary**: a remote machine can invoke local tools. To bound the blast radius:

- **Enforced locally (egress).** Each machine's federation module is the sole gatekeeper for **that machine's** tools. Home decides what home re-exports to each peer. If home exposes only readers to the VPS, the VPS cannot see or call mutators on home. There is nothing to attack and no central trust required. Enforcement is symmetric (each side controls its own exposure).
- **Default posture reuses the `execution_mode` taxonomy** already on every `Tool` manifest (`pure | mutating | unfenceable`): `pure` tools are **federatable by default**; `mutating` and `unfenceable` require **explicit per-peer opt-in** in the profile. A compromised VPS gets your readers for free and cannot touch a mutator on home unless that exact peer was deliberately allowed for that class.

We are not inventing a new trust label; we route the boundary policy through `execution_mode`, which already classifies exactly the danger we care about.

### 2.4 F5 — one federation module, two profile sources
Manual pairing (CK app exchanges a pairing token, fully self-hosted, no cloud) and cloud-login produce the **same profile**; only the provisioning source differs. We build **one** federation module with a pluggable profile source. The free self-hosted path and the paid cloud path share the same data plane and never diverge.

### 2.5 The unifying invariant — federation module = sole network endpoint
**subc-core is always loopback-only on every machine.** The federation module is the only component that ever touches the network. Every cross-machine path terminates at the home machine's federation module, which bridges relayed/peer traffic onto a **loopback** connection to the local subc. Whether the remote party is another subc (peer federation) or a CK app via relay (roaming client), subc-core only ever sees a loopback connection, exactly as today. This is the same quarantine principle as §2.1, generalized to all network parties.

---

## 3. Architecture

### 3.1 The federation module (the "trunk line")
A subc module, supervised like any other (`subc.jsonc`), present on each **full** node. On each machine it is simultaneously:

- **A local provider** that re-exports each connected peer's tools into the **local** catalog, namespaced by origin (e.g. `home:aft_read`, `vps:status`). When a local agent routes to a re-exported tool, subc routes to the federation module exactly like any provider.
- **A network bridge** to the data plane: it dials/accepts encrypted connections to peer federation modules (and to the cloud relay when needed), forwards opaque tool-call bodies across, and translates channels between the local subc and the network leg.

subc-core never knows a call left the machine. The opaque-body invariant holds end to end: the federation module maps **envelope channels** (like subc itself) but does not parse tool **bodies**.

### 3.2 Control plane vs data plane
- **Control plane (cloud, optional):** authenticates the user (single login), holds the network map (which devices belong to the account + their public keys + reachability hints), distributes per-peer policy/ACLs, and handles device enrollment/revocation. It is the rendezvous and identity broker. It is NOT in the tool-call path.
- **Data plane (always):** the encrypted peer-to-peer (or relayed) tunnels carrying actual tool calls. End-to-end between the two endpoints; any relay forwards ciphertext only.

In the **manual-pairing** (no-cloud) mode, the control-plane functions (identity exchange, peer list, policy) are performed by the CK app via a pairing token; the data plane is identical.

### 3.3 Profiles & identity
A **profile** is the federation membership + policy descriptor. Working scope (open for refinement, see §8 Fork 4):

- **This node's identity** (machine identity; for roaming clients, a per-device identity).
- **Peers**: the set of other nodes this node federates with, their endpoints/reachability, and their public keys.
- **Egress policy (per peer)**: which local modules/tools are exposed to each peer, defaulting per §2.3 (`pure` exposed, `mutating`/`unfenceable` opt-in).
- **Device keys**: per-device, not just per-machine, so a lost phone can be revoked without re-keying everything (see §6.4).

The CK app manages profiles. Cloud-login distributes one; manual pairing assembles one locally. The profile is the single source of truth the federation module reads.

### 3.4 Node taxonomy — peers vs leaves
- **Peers** (laptop, VPS): run a full subc daemon + a federation module; can host tools/agents and hold tunnels. Symmetric participants.
- **Leaves** (phone, iPad): run **no** daemon (iOS/iPadOS won't host a background subc). They are **thin relayed consumers**: the native Swift subc client in consumer mode, dialing the relay to reach a home peer. A leaf can drive and observe a peer's tools/agents but cannot host any. The subc-transport HMAC handshake runs **end-to-end** leaf↔home over the relay, so home authenticates the leaf by key-possession exactly like a local client.

---

## 4. Data flows

### 4.1 Catalog federation
1. Federation module on A establishes a data-plane connection to peer B's federation module.
2. A's module learns B's exposed catalog (the subset B's egress policy permits) and re-registers those tools into A's local subc under the `B:` namespace via its provider HELLO.
3. Catalog changes on B (module registers/dies) propagate to A's module, which updates its re-exported provider role. Liveness/staleness across the network is a federation-module concern (see §7).

### 4.2 Cross-machine tool call (VPS agent calls a home tool)
```
[Agent on VPS] → VPS subc → VPS federation-module (registered provider for `home:*`)
              → (data plane, E2E) → Home federation-module
              → loopback → Home subc → [home AFT tool]
              → response retraces the path back
```
subc-core on both machines does only its normal channel routing. The federation modules own the network leg + channel translation + the egress policy gate (home's module refuses a call to a tool not exposed to the VPS peer).

### 4.3 Roaming client reaches home (iPad via relay)
```
[CK app on iPad] (native Swift consumer, relay-dial transport)
   → Cloud relay (rendezvous + auth broker; forwards ciphertext only)
   → Home federation-module (holds the persistent outbound tunnel)
   → loopback → Home subc → home tools / agents
```
- The relay is the **normal** path here (neither the iPad nor home has a direct route), but it is still E2E: the iPad and home share keys (provisioned via cloud login / pairing), so the relay cannot read the traffic.
- Reuse: the subscribe-streaming already shipped in the Swift client lets the iPad **watch a home agent's live token stream over the relay** (start a long task at home, monitor/steer from the train).

---

## 5. Security model

1. **Trust boundary inversion is the central risk.** Today any local key-holder is trusted (same-host). Federation lets a remote machine invoke local tools; a popped VPS could otherwise `bash`/`write`/`edit` on home.
2. **Default-deny, egress-enforced-locally, keyed on `execution_mode`** (§2.3) is the primary mitigation. The exposure surface to any peer is exactly what that peer's profile entry allows; the default excludes all mutators.
3. **E2E through any relay** (§2.2): the cloud relay forwards ciphertext, never plaintext tool payloads.
4. **subc-core stays loopback-only** (§2.5): no machine ever binds a non-loopback socket in subc-core; only the federation module touches the network, so the audited same-host transport is unchanged.
5. **Device identity + revocation** (§6.4): per-device keys mean a lost/compromised device is revoked centrally without disrupting other nodes.

---

## 6. Reliability & correctness (must be addressed; framed for council)

### 6.1 At-most-once across the network hop
The cross-machine leg adds a new failure/ambiguity window on top of subc's local model. The federation module must map network failures into the **existing** `NotSent` / `OutcomeUnknown` taxonomy (the same one `subc-client-rs` / `@cortexkit/subc-client` already use for managed calls), so the mutation-safety guarantee (never blind-retry an `OutcomeUnknown` mutation) holds across machines, not just locally. A pre-send network drop is `NotSent` (safe to retry); a post-send/pre-ack drop is `OutcomeUnknown` (never auto-retry a mutator).

### 6.2 Partition & liveness
- Remote catalog **staleness**: how fast must a remote module's death reflect in the local catalog? On partition, the federation module should surface re-exported remote tools as unavailable rather than hang.
- **In-flight cross-machine calls on partition**: must settle deterministically (route-gone / `OutcomeUnknown`), reusing the route-GOODBYE / channel-gone contracts already defined for the local case.

### 6.3 Relay as product-critical infra
For the roaming-client case the relay is the normal path, not a degraded fallback. Running reliable relays is an operational commitment (availability, capacity, geographic distribution). This is a real cost/ops decision, not just code.

### 6.4 Device identity & revocation
Per-device keys in the control plane. Losing a phone means revoking that device's key from the cloud (Tailscale-style device revocation), invalidating its access without re-keying other nodes. The profile/identity layer (Fork 4) must model per-device, not just per-machine.

---

## 7. Reference model

This problem is closely analogous to **Tailscale**: a coordination server (identity, key distribution, ACLs, device management) plus a WireGuard data plane with a DERP relay fallback, data direct-when-possible. We deliberately mirror that split (§2.2). Where we differ: our "nodes" are subc daemons and the unit of access is a **tool/agent capability** (gated by `execution_mode`), not raw IP connectivity.

---

## 8. Open forks (for council to pressure-test + help resolve)

**Fork 3 — NAT / who dials whom.** The VPS has a public IP; home is behind NAT (no inbound). Lean: the **reachable** peer is the server, the **NAT'd** peer holds a persistent outbound tunnel, and the cloud relay is used only when neither side is directly reachable (e.g. both behind NAT, or roaming leaf). Open: hole-punching vs always-relay for the home-behind-NAT case; keepalive/reconnect strategy for the held tunnel.

**Fork T — data-plane transport: WireGuard vs mTLS.** Lean: **mTLS directly in the federation module for v1** (two-node laptop+home / VPS+home; tool RPC is request/response, not bulk; no WireGuard dependency or kernel/userspace tunnel management). **WireGuard** becomes compelling once it is a real mesh (many nodes, hole-punching, transport-level encryption + NAT traversal for free). Open: is the v1 two-node assumption safe, or do we want WireGuard from the start to avoid a transport migration later?

**Fork 4 — profile schema (now includes per-device keys).** Confirm the profile scope (§3.3). Open: is a profile purely federation membership + policy, or does it also bundle per-environment model/auth/tool config? How are profiles versioned and reconciled when cloud-distributed vs locally-paired?

**Fork C — cloud control-plane trust.** Single-login convenience means the cloud mints/distributes credentials, so in principle it could impersonate a device or inject a rogue peer. Mitigations to evaluate: the cloud distributes **public** keys only (data stays E2E so it cannot decrypt), device enrollment requires a device-held secret (cloud cannot self-enroll a device), and policy is signed. Open: exact trust assumptions, and whether manual-pairing must remain fully cloud-independent for the privacy-max tier.

**Fork Cat — catalog sync mechanics.** Poll vs subscribe for remote catalog changes; the acceptable staleness window; how namespacing collisions across many peers resolve (reuse subc-mcp's namespace reverse-map?).

**Fork P — partition semantics.** Precise behavior for in-flight cross-machine calls and re-exported catalog entries on network partition and reconnect (§6.2).

---

## 9. Proposed build phasing (de-risk smallest first)

1. **Spike: two-peer direct federation, no cloud.** Two subc daemons on two reachable hosts (or two ports on one box), federation module pair, manual key exchange, catalog re-export + a single cross-machine `pure` tool call. Proves the module-pair shape, channel translation, and the loopback-only invariant. mTLS data plane.
2. **Egress policy + `execution_mode` scoping.** Per-peer exposure, default-deny, mutating opt-in. Adversarial test: a peer cannot reach a non-exposed mutator.
3. **At-most-once + partition handling** across the hop (§6.1/§6.2).
4. **NAT'd home + held outbound tunnel** (Fork 3), then **relay** for the no-direct-path case.
5. **Roaming leaf (mobile/iPad)** via relay: native Swift consumer relay-dial transport, E2E handshake through the relay, subscribe-streaming over the relay.
6. **Cloud control plane**: single-login identity, network-map distribution, device enrollment + revocation. The data plane from phases 1-5 is unchanged; this only adds a profile source.

Each phase is independently demonstrable, and the data plane (phases 1-3) is shared by both the manual and cloud tiers (§2.4).

---

## 10. Decision log

- **Federation (A) + InterSUBC quarantine** — prior, carried forward (§2.1).
- **F1 Tailscale-style control/data split; cloud never sees payloads** — locked (§2.2).
- **F2 default-deny per-peer scoping, egress-enforced-locally, keyed on `execution_mode` (pure federatable, mutating/unfenceable opt-in)** — locked (§2.3).
- **F5 one federation module, two profile sources (manual pairing + cloud login)** — locked (§2.4).
- **Federation module = sole network endpoint; subc-core always loopback-only** — locked (§2.5).
- **Roaming client = relay path promoted to first-class; E2E preserved; thin Swift consumer leaf** — locked (§4.3, §3.4).
- **Fork 3 (NAT), Fork T (transport), Fork 4 (profile schema), Fork C (cloud trust), Fork Cat (catalog sync), Fork P (partition)** — OPEN, for council (§8).
