# subc Federation & Profiles — Design Plan

**Status:** v2 — the three architecture forks (exposure axis / transport / loopback-for-N-peers) resolved by Ufuk; all 6 council BLOCKERs folded with resolutions. Re-gate pending to confirm blockers closed. Several lower-severity forks remain open (§8).
**Owner:** subc (Alfonso @ subconscious).
**Scope:** Cross-machine subc-to-subc federation, the profiles/identity layer, the optional cloud control plane, and the roaming-client (mobile/iPad) relay path. Future subsystem; nothing built yet.

## v1 → v2 changelog (what the council round changed)
A 5/5 adversarial council found 6 BLOCKERs. Three were architecture forks routed to Ufuk and are now resolved; three were mine to resolve and are drafted here:
- **(a) Exposure axis — RESOLVED.** `execution_mode` was the wrong axis (it classifies retry-safety, not confidentiality; `pure` readers leak files/secrets). Replaced by a dedicated `federation_exposure`, default deny-all (§2.3).
- **(b) Transport / E2E — RESOLVED.** "mTLS in the module" + "relay never sees plaintext" were contradictory (subc-transport is symmetric-HMAC + plaintext frames; a relay terminating mTLS sees plaintext). Data plane is now a **Noise IK** end-to-end tunnel, per-device keys, cloud holds public keys only (§2.2). Explicitly **not** replicating Tailscale/WireGuard (scope is tool/agent federation, not a general service fabric).
- **(c) Loopback-for-N-peers — RESOLVED.** subc-core can't multiplex N namespaced providers on one connection (`register_module_connection` evicts the prior registration, forwarding.rs:252-254; u16 channels are per-connection). Resolution: **one loopback connection per peer**, keeping subc-core unchanged (§2.5).
- **(#3 at-most-once across the hop), (#4 catalog liveness/partition), (#5 reserved fed-module + namespace-to-pubkey)** — drafted in §5/§6, plus the cross-cutting gaps (cross-version negotiation, relay DoS, audit trail, remote-BindIdentity confused-deputy, per-device keys).

---

## 1. Motivation & use cases

Users will run subc on more than one machine and want their tools and agents to feel like one fabric:

1. **Peer federation (laptop ↔ VPS).** Some agents run on the VPS, some on the computer; an agent on either must call a tool registered on the other, seamlessly. The CK app sets it up: install subc on both, connect, discover catalogs, transparent cross-machine calls.
2. **Cloud single-login federation.** A single account login makes a user's subc instances discover and connect automatically. The convenience tier (plausibly paid) over the same mechanism.
3. **Roaming client reaches home (mobile / iPad).** An unsavvy user reaches their home subc from the CK app on a phone/iPad when away, with zero networking setup, via a small cloud relay endpoint, securely.

Common thread: **make a user's tools and agents reachable across machines, securely, with zero manual networking, without weakening subc's same-host security model or thickening subc-core.**

---

## 2. Locked decisions

### 2.1 Foundation (prior decision, carried forward)
**Federation (A):** every machine runs its own subc + local modules, **loopback-only**, key-on-disk, same-host threat model. WAN traffic is **quarantined to a subc-supervised InterSUBC federation module**, keeping modules network-unaware. (Architecture doc §11 decision log; exercised as the `federation` archetype in `crates/subc-core/tests/closure.rs`.)

### 2.2 F1 — Tailscale-style control/data split, Noise IK data plane
The cloud is a **control plane only**: identity, discovery, policy distribution. **Tool-call bytes go peer-to-peer directly**; the cloud relays data **only** as a NAT/roaming fallback and **never sees tool payloads**.

**Hard line preserved by a real E2E layer.** Because subc-transport is symmetric-HMAC with plaintext frames (it has to be: the splice router reads the plaintext envelope to route), a relay-terminating transport would see plaintext. So the WAN data plane is a **Noise IK** end-to-end tunnel between the two endpoints:
- **Per-device asymmetric keypairs.** The cloud is a **public-key directory only** (it never holds a secret that lets it decrypt or impersonate).
- The session key is derived **end-to-end** between the two endpoints; any relay forwards **ciphertext it cannot read**.
- Noise also gives per-frame integrity + anti-replay (which symmetric-HMAC-then-plaintext does not), and per-device keys give clean revocation.

**Why Noise, not WireGuard/Tailscale (scope decision, Ufuk):** WireGuard's crypto core *is* Noise IK; WireGuard = Noise IK + an IP-tunnel device + key/peer mgmt. We want the security core, not the IP-tunnel packaging: (1) WireGuard does **not** provide discovery/NAT-traversal/relay — those are our cloud control plane either way; (2) an IP-tunnel device is **friction exactly in the unsavvy-mobile case** (iOS needs a Network Extension entitlement; a Noise channel in the app is just userspace crypto over a socket); (3) our transport unit is the tool-RPC stream, not arbitrary IP traffic. WireGuard/Tailscale-grade would be the end-state only if CortexKit federates **arbitrary services** (web/SSH/db) — a general-VPN product, not the current tool/agent scope. If that vision lands, revisit adopting Tailscale/headscale rather than rebuilding. The expensive layer (coordination/relay/identity) is reusable regardless, so this is not a throwaway-and-migrate.

### 2.3 F2 — default-deny `federation_exposure` (NOT `execution_mode`)
Federation **inverts subc's trust boundary** (a remote machine can invoke local tools), and `execution_mode` is the **wrong axis** for the boundary: it classifies **retry-safety** (`pure` = safe to re-run), not **confidentiality**. A `read`/`grep`/`search`/env-dump/vault-read is `pure` yet exposes file contents and secrets, so "pure federatable by default" would expose the entire read/exfiltration surface with zero opt-in. A popped VPS could read every file on home without touching a mutator.

Resolution — a dedicated, explicit exposure control:
- **`federation_exposure`, default deny-all (`local_only`).** Nothing crosses the boundary unless explicitly allowed. Covers **all** exported roles — tools **and** ManagementSurface ops (which have no `execution_mode` at all).
- **Per-peer allow-list in the profile**, authored via the **CK app**. The user chooses what each peer can reach.
- **Premade policy templates** (later): named bundles that expand to a per-tool allow-set over the deny-all base (e.g. "read-only peer", "trusted workstation", "full"). Sugar over the per-tool/per-peer primitive.
- **Per-tool "never-federatable" hard floor** in the manifest: some tools (e.g. raw shell) are never exposable regardless of profile.
- `execution_mode` still has a federation job, a **different** one: the at-most-once **reliability** gate (§6.1) — a `mutating`/`unfenceable` tool over the WAN must never be auto-retried on an ambiguous outcome. Two axes, two jobs: `federation_exposure` = *may it cross?*, `execution_mode` = *is it safe to retry across the hop?*

### 2.4 F5 — one federation module, two profile sources
Manual pairing (CK app exchanges a pairing token, fully self-hosted, no cloud) and cloud-login produce the **same profile**; only the provisioning source differs. One federation module, a pluggable profile source. Free self-hosted path and paid cloud path share the data plane and never diverge.

### 2.5 The unifying invariant + one-connection-per-peer
**subc-core is always loopback-only on every machine.** The federation module is the **only** component that ever touches the network; every cross-machine path terminates at the home machine's federation module, which bridges onto a **loopback** connection to the local subc. Whether the remote party is another subc (peer federation) or a CK app via relay (roaming client), subc-core only ever sees loopback, exactly as today.

**One loopback connection per peer** (resolves the N-peer concern with zero subc-core change): the federation module opens one loopback connection to the local subc **per remote peer**, each HELLO-registering that peer's namespaced catalog (`vps1:*`, `vps2:*`). This keeps per-peer isolation (separate u16 channel spaces; one peer's reconnect never disturbs another) and uses subc-core's existing connection-id-keyed multi-registration (the same property that lets one process hold a provider + consumer connection). Accepted v1 coarseness: a peer's catalog change re-HELLOs **that peer's** connection (evict + re-register), briefly dropping its routes — but the blast radius is contained to the one affected peer (the multiplex alternative would churn all peers and require a subc-core change). A finer incremental-catalog mechanism is a future subc enhancement, not a v1 blocker.

---

## 3. Architecture

### 3.1 The federation module (the "trunk line")
A subc module, supervised like any other, present on each **full** node. It is **mandated `reserved` + launch-nonce-bound** (§5.2) so a rogue local key-holder cannot impersonate it. On each machine it is simultaneously:
- **A local provider (one loopback connection per peer)** that re-exports each peer's *exposed* catalog into the local catalog, namespaced by the **verified peer public key** (the human-readable name is display sugar; the authoritative namespace key is the pubkey, §5.2).
- **A network bridge**: it establishes **Noise IK** sessions to peer federation modules (direct, or through the cloud relay), forwards opaque tool-call bodies, and translates channels between the local subc and the network leg.

subc-core never knows a call left the machine. The opaque-body invariant holds end to end: the module maps **envelope channels** (like subc) but never parses tool **bodies**.

### 3.2 Control plane vs data plane
- **Control plane (cloud, optional):** authenticates the user (single login), holds the network map (which **devices** belong to the account + their **public** keys + reachability hints), distributes per-peer policy/ACLs (**signed**, §6.2), and handles **device enrollment + revocation**. Rendezvous + identity broker. NOT in the tool-call path; holds **no** secret that decrypts traffic.
- **Data plane (always):** Noise IK end-to-end tunnels carrying tool calls. E2E between the two endpoints; any relay forwards ciphertext only.
- **Manual-pairing mode:** the CK app performs the control-plane functions (identity exchange via a pairing token, peer list, policy); the data plane is identical.

### 3.3 Profiles & identity
A **profile** is the federation membership + policy descriptor:
- **This node's identity** (per-**device** keypair, not just per-machine — §6.4).
- **Peers**: each peer's public key + reachability; the authoritative peer identity is the **public key**, names are display-only.
- **Egress policy (per peer)**: the `federation_exposure` allow-list (§2.3) — which local tools/surfaces are exposed to each peer; default deny-all.
- The CK app authors profiles; cloud-login distributes one (signed); manual pairing assembles one locally. The profile is the single source of truth the federation module reads.

### 3.4 Node taxonomy — peers vs leaves
- **Peers** (laptop, VPS): full subc daemon + federation module; host tools/agents and hold tunnels. Symmetric.
- **Leaves** (phone, iPad): **no** daemon. Thin relayed consumers — the native Swift subc client in consumer mode, dialing the relay to reach a home peer over a Noise session. Can drive/observe a peer's tools/agents, cannot host. The Noise handshake runs **end-to-end** leaf↔home through the relay, so home authenticates the leaf by its device key, and the leaf gets the same `federation_exposure` gating as any peer.

---

## 4. Data flows

### 4.1 Catalog federation
1. The federation module on A establishes a Noise session to peer B's federation module.
2. A learns B's **exposed** catalog (the subset B's `federation_exposure` permits) and re-registers it into A's local subc under the `B:` (pubkey-keyed) namespace via a per-peer provider HELLO.
3. Catalog changes on B propagate to A, which re-HELLOs B's connection (§2.5). Liveness/staleness is bounded and signed (§6.2).

### 4.2 Cross-machine tool call (VPS agent calls a home tool)
```
[Agent on VPS] → VPS subc → VPS federation-module (provider for `home:*`)
              → Noise IK session (direct or relay, E2E) → Home federation-module
              → loopback → Home subc → [home tool]
              → response retraces the path
```
- subc-core on both machines does only normal channel routing.
- Home's module enforces `federation_exposure` (refuses a call to a tool not exposed to the VPS peer) **and** injects the **local** BindIdentity from the profile — it never trusts a remote-supplied identity (§5.4 confused-deputy fix).
- At-most-once is governed end-to-end (§6.1).

### 4.3 Roaming client reaches home (iPad via relay)
```
[CK app on iPad] (native Swift consumer, Noise over relay-dial)
   → Cloud relay (rendezvous + auth-before-resource; forwards ciphertext only)
   → Home federation-module (holds the persistent outbound tunnel)
   → loopback → Home subc → home tools / agents
```
- The relay is the **normal** path here (neither side has a direct route), still E2E: the iPad and home derive the Noise session key end-to-end; the relay can't read it.
- Reuse: the Swift client's subscribe-streaming lets the iPad **watch a home agent's live token stream over the relay**.

---

## 5. Security model

1. **Trust-boundary inversion is the central risk** (a remote machine invoking local tools).
2. **Default-deny `federation_exposure`, egress-enforced-locally** (§2.3), covering tools AND management surfaces; the exposure surface to any peer is exactly its profile allow-list.
3. **E2E via Noise IK** (§2.2): relay forwards ciphertext; per-frame integrity + anti-replay; cloud holds public keys only.
4. **subc-core stays loopback-only** (§2.5): no non-loopback bind in subc-core; only the federation module touches the network, so the audited same-host transport is unchanged.

### 5.2 Reserved federation module + namespace-to-pubkey binding (BLOCKER #5)
HELLO authorizes any non-empty `module_id` (no per-module authz), so a local key-holder could impersonate the federation module or squat a peer namespace and become the ingress/egress bridge. Resolutions:
- **Mandate the federation module `module_id` as `reserved` + launch-nonce-bound** (the same primitive shipped for the credentials vault): only the daemon-spawned, nonce-matching process can register it.
- **Reserve peer-namespace prefixes**, and bind each exported namespace to the **verified peer public key**, not an arbitrary module name. Human-readable names are display-only and spoofable; the pubkey is the authoritative key.

### 5.3 Cloud control-plane trust
Single-login convenience means the cloud distributes credentials, so it must not be able to impersonate. Mitigations baked in: the cloud distributes **public keys only** (data stays E2E, it can't decrypt), **device enrollment requires a device-held secret** (the cloud can't self-enroll a device), and **policy is signed** (a tampered network-map/ACL is detectable). Manual-pairing remains fully cloud-independent for the privacy-max tier.

### 5.4 Remote-BindIdentity confused-deputy
The federation module **injects the local identity from the profile** and never forwards a remote-supplied `BindIdentity` (project_root/harness/session) into the local subc — otherwise a remote peer could steer a local tool to read the wrong project. Remote requests carry only the tool + args; identity is local-stamped.

### 5.5 Relay abuse / DoS
The relay does **auth-before-resource** (a connection proves device identity before any forwarding state is allocated) and enforces **per-account/per-device quotas + rate limits**, so an attacker can't amplify or exhaust the relay.

### 5.6 Cross-machine audit trail
Each cross-machine call emits a **tamper-evident local audit record** on the serving machine (which peer invoked which tool with what identity) — the central-risk action must be observable after the fact.

---

## 6. Reliability & correctness

### 6.1 End-to-end at-most-once across the hop (BLOCKER #3)
Local at-most-once (`NotSent` / `OutcomeUnknown`) **does not compose** across consumer→fed-module→[relay]→remote: local classification keys on local-socket "accepted", which collapses the remote leg's `NotSent` into the consumer's `OutcomeUnknown`, and there is no durable cross-machine send-log — so mutations can be lost-but-marked-unretryable, or auto-retried-and-duplicated. Resolution — an explicit **end-to-end** state machine:
- **Durable correlation IDs** minted end-to-end (not per-hop), and a **durable outbound send-log** in the federation module.
- The fed-module **does not report `accepted`** to the local consumer until the send is **durably recorded**; it **never auto-retries post-accept** on the WAN.
- **Mutators carry a federated idempotency/fencing token** so a legitimately retryable `NotSent` re-send cannot double-apply, and an `OutcomeUnknown` is surfaced (never blind-retried), consistent with the existing client taxonomy. `execution_mode` (§2.3) is the gate that decides which calls demand this strict path.

### 6.2 Partition, liveness & catalog freshness (BLOCKER #4)
Re-exported catalog liveness + subc's silent-drop (no reactive NACK) means a cross-machine call during a staleness window can **vanish with no classification** (worse than `OutcomeUnknown`) or hang to timeout. Resolutions:
- **Per-peer keepalive** with a **bounded numeric staleness window**; on a missed keepalive the module marks the peer's re-exported tools unavailable rather than letting calls hang.
- **GOODBYE-on-partition**: in-flight cross-machine calls are settled deterministically (route-gone / `OutcomeUnknown`) on partition, reusing the local route-GOODBYE / channel-gone contracts.
- **Signed catalog/policy generations + tombstones**, with a **per-call policy-version check** so a stale exposure policy can't be exploited across a generation change.

### 6.3 Relay as product-critical infra
For the roaming case the relay is the **normal** path, not a fallback — an operational commitment (availability, capacity, geo-distribution). Real ops cost, flagged.

### 6.4 Device identity & revocation
Per-device keypairs in the control plane. Losing a phone → revoke that device's key from the cloud (and propagate a signed revocation/tombstone); other nodes are unaffected. `ClientHello` must carry a **device identity** so per-device keying/revocation is implementable (today it does not — a v2 transport addition for the federation/leaf path).

### 6.5 Cross-version federation
Two federated nodes may run different `subc-protocol` versions (e.g. an unknown `execution_mode` variant would be a deserialize error). The federation handshake performs **version/capability negotiation** and applies manifest-compat rules (unknown fields tolerated, unknown roles/ops excluded from exposure) so a newer peer can't break an older one.

---

## 7. Reference model

Closely analogous to **Tailscale**: a coordination server (identity, key distribution, ACLs, device mgmt) + a Noise-based data plane (WireGuard's core is Noise IK) with relay fallback, direct-when-possible. We deliberately take the **Noise IK security core** and our own thin relay, and **not** the IP-tunnel fabric (§2.2 scope decision). Our unit of access is a **tool/agent capability** (gated by `federation_exposure`), not raw IP connectivity.

---

## 8. Forks

**RESOLVED by this revision:**
- Exposure axis → dedicated `federation_exposure`, default deny-all (§2.3).
- Data-plane transport → Noise IK E2E (Fork T closed; mTLS-at-module is out, WireGuard deferred to a possible general-fabric future) (§2.2).
- Loopback-for-N-peers → one connection per peer (§2.5).

**STILL OPEN (for the re-gate + later calls):**
- **Fork 3 — NAT specifics.** Lean: reachable peer is server, NAT'd peer holds the outbound tunnel, relay when neither is reachable. Open: hole-punching vs always-relay for home-behind-NAT; keepalive/reconnect tuning for the held tunnel.
- **Fork 4 — profile schema depth.** §3.3 scope confirmed; open: versioning/reconciliation of cloud-distributed vs locally-paired profiles, and whether a profile also bundles per-environment model/auth/tool config.
- **Fork Cat — catalog-sync granularity.** v1 is coarse re-HELLO per peer (§2.5); open: whether/when to add incremental catalog deltas as a subc enhancement, and the acceptable staleness window number.
- **Fork C — cloud trust depth.** §5.3 mitigations stated; open: exact enrollment-secret mechanism and signed-policy key management.

---

## 9. Build phasing (de-risk smallest first)

1. **Spike: two-peer direct federation, no cloud.** Two daemons on two reachable hosts (or two ports), federation-module pair, **Noise IK** session, manual key exchange, **one loopback connection per peer**, catalog re-export + a single cross-machine **exposed** tool call. Proves the module-pair shape, channel translation, the loopback-only invariant, and a confirming **multi-registration-per-process** test. Reserved + nonce-bound module from the start.
2. **`federation_exposure` enforcement.** Per-peer deny-all + allow-list; adversarial test: a peer cannot reach a non-exposed tool or management surface; remote-BindIdentity is local-injected (§5.4).
3. **End-to-end at-most-once + partition** (§6.1/§6.2): durable correlation IDs + send-log, fencing for mutators, keepalive + GOODBYE-on-partition, signed catalog generations.
4. **NAT'd home + held outbound tunnel** (Fork 3), then **relay** for the no-direct-path case (auth-before-resource + quotas).
5. **Roaming leaf (mobile/iPad)** via relay: native Swift consumer Noise-over-relay, E2E through the relay, subscribe-streaming.
6. **Cloud control plane**: single-login identity, signed network-map distribution, device enrollment + revocation. The data plane from 1-5 is unchanged; this only adds a profile source + key directory.

Each phase is independently demonstrable; the data plane (1-3) is shared by the manual and cloud tiers.

---

## 10. Decision log
- **Federation (A) + InterSUBC quarantine** — prior, carried forward (§2.1).
- **F1 Tailscale-style split; cloud never sees payloads; Noise IK E2E data plane, per-device keys, cloud = public-key directory; NOT WireGuard/Tailscale-fabric (tool/agent scope, not general VPN)** — locked (§2.2, Ufuk).
- **F2 default-deny `federation_exposure` (not `execution_mode`); per-peer allow-list via CK app; templates later; per-tool never-federatable floor; covers management surfaces; `execution_mode` retained only for the at-most-once reliability gate** — locked (§2.3, Ufuk).
- **F5 one federation module, two profile sources** — locked (§2.4).
- **Federation module = sole network endpoint; subc-core always loopback-only; one loopback connection per peer** — locked (§2.5, Ufuk).
- **Reserved + nonce-bound fed-module; namespace bound to peer pubkey** (§5.2); **end-to-end at-most-once w/ durable send-log + fencing** (§6.1); **keepalive + bounded staleness + GOODBYE-on-partition + signed generations** (§6.2); **local-injected BindIdentity** (§5.4); **relay auth-before-resource + quotas** (§5.5); **per-device keys + revocation** (§6.4); **cross-version negotiation** (§6.5) — drafted from council BLOCKERs, **pending re-gate**.
- **OPEN:** NAT specifics, profile-schema depth, catalog-sync granularity, cloud-trust depth (§8).
