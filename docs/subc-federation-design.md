# subc Federation & Profiles — Design Plan

**Status:** v4 — the v3 re-gate (4/4 council, unanimous on all three phase-0 blockers) confirmed the architecture but found P1/P2 under-specified and §6.1 carrying durability residues. v4 re-scopes P1 (provides-list-only, no tool-granular GOODBYE promise), fully specifies P2 (delimiter semantics + honest same-user threat-model statement), closes the at-most-once holes (incarnation epoch, recovery reconciliation, retention co-definition, explicit CallError mapping), and pins the TOFU ceremony. Re-gate pending on v4; phase 0 gated on it.
**Owner:** subc (Alfonso @ subconscious).
**Scope:** Cross-machine subc-to-subc federation, the profiles/identity layer, the optional cloud control plane, and the roaming-client (mobile/iPad) relay path. Future subsystem; nothing built yet.

## v3 → v4 changelog (what the v3 re-gate changed)
4/4 council members converged on the same findings. The fixes, all folded below:
- **P1 re-scoped (#1, #2 — unanimous blockers):** "routes to vanished tools get GOODBYE" was architecturally impossible (routes bind module endpoint + channel; tool names live only in opaque bodies — forwarding.rs:43-60). P1 now updates **only the `provides` tools list**; a call to a removed tool gets a **module-side typed error, never a GOODBYE** (the module is authoritative for its own dispatch — same as local behavior today). Frozen fields (module_id, role kind, concurrency, control_ops) are HELLO-time properties; a P1 payload changing them is **rejected** (`catalog_update_frozen_field`) — changing those requires a full drain + re-HELLO. This keeps the flow-control window and the health prober's op view consistent by construction.
- **P2 fully specified (#3 — unanimous blocker):** delimiter-aware prefix semantics (reserved prefixes MUST end with `:`; match = `id.starts_with(prefix)`; exact-id reservations take precedence; overlapping owners rejected at config load; boundary-case test matrix). Ownership maps **prefix → owner supervised module_id**, verified against the owner's current spawn nonce. **Honest threat-model statement:** P2 is NOT a same-user barrier and cannot be — subc's same-host model already makes a same-user process all-powerful (it can read the connection key file and impersonate anything); P2 protects against accidental collisions and different-user/different-trust-tier processes, exactly like the existing exact-id reservation. Which PEER namespaces the fed-module may create under `fed:` is fed-module policy, not P2's job.
- **§6.1 holes closed (#4, #5, #6):** effect_id gains a durable **incarnation epoch** — `(origin_device_pubkey, incarnation_uuid, seq)` — so origin-DB loss can never collide; serving side fences seq regression within an incarnation. The pre-intent-crash hole is closed by **recovery reconciliation**: on restart the origin fed-module queries the serving ledger for every `sent`-without-outcome effect and settles it; an explicit **fed-state → CallError mapping table** defines what the origin consumer sees in every window. Ledger retention is **co-defined** with the origin send-log (retain until origin confirms outcome received + grace; post-expiry re-arrival = typed ambiguity refusal, never re-dispatch, never replay). The llm-runner reputation appeal is dropped — the mechanics are standard WAL discipline and stand on their own.
- **Peer topology decided (#11):** one loopback connection per **(peer, remote module)** — not per peer, not one synthetic module — preserving role fidelity and per-module P1 updates; one HELLO per connection (matches the eviction semantics).
- **Partition classifier (#9):** the fed-module's keepalive reaper is the authoritative partition classifier; it settles in-flight calls by **closing the affected per-peer loopback connections** (connection-granular cleanup → deterministic GOODBYE to consumers); subc's module-direction GOODBYE stays the best-effort hint it is.
- **Cross-version (#10):** the federation handshake exchanges **raw capability docs**; the fed-module filters/translates to the negotiated version BEFORE constructing the typed local manifest (a closed ProviderRole enum cannot "skip" unknown roles at decode).
- **Harness story (#7):** federated calls use a first-class `fed:<peer-fingerprint>` harness value; providers' harness allowlists must admit the `fed:` class (AFT coordination item, verified against real AFT — phase-2 gate). Peer identity rides the harness; per-peer session scoping rides the session field.
- **TOFU ceremony (#8):** cloud-introduced pairs are **non-routable until the out-of-band code is compared** (friction once, at introduction); rotation requires a chain signed by the old key OR re-verification identical to first contact; the code binds **both endpoints' long-term device static keys**; residual documented (a user who dismisses the code prompt on the cloud tier is vulnerable — accepted convenience-tier cost; manual pairing is structurally immune).
- **Fork Cat contradiction fixed (#13); ClientHello device-identity confirmed phase-4+ (#12).**

## v2 → v3 changelog (what the re-gate changed)
The v2 re-gate confirmed the architecture skeleton but rejected two v2 claims against the actual subc-core source and demanded mechanics for three hand-waved areas:
- **(1, code-verified) Coarse re-HELLO is not viable, so v2's "accepted v1 coarseness" is withdrawn.** `Registry::register_with_control_ops` rejects a duplicate `module_id` (registry.rs:75), so refreshing a peer's catalog requires dropping the peer's whole connection first (disconnect → cleanup → reconnect → HELLO). That is not "briefly dropping routes": every in-flight cross-machine call on that peer dies on every catalog change, and catalog changes are routine (any module restart on the remote). Resolution: **subc-core primitive P1, non-disruptive catalog update** (§2.6).
- **(2, code-verified) Peer-namespace squatting is real.** Reserved-nonce protection gates **exact** module ids only (supervise.rs `reserved_nonces` map); peer catalogs register as namespaced ids under the federation module's connections, so any local key-holder could HELLO-register a peer-namespaced id and become the apparent bridge for that peer. Resolution: **subc-core primitive P2, namespace-prefix reservation** (§2.6).
- **(3) At-most-once was direction, not mechanics** → §6.1 now specifies the send-log schema, fsync barriers, the remote dedup ledger, and the end-to-end state machine (standard WAL intent-log discipline).
- **(4) Caller-identity and target-selection were conflated** → §5.4 now separates the two: the remote peer's identity (authenticated pubkey) selects the *policy*, while the local BindIdentity stamped on the bind is *profile-authored*, never remote-derived — and the serving module records both in the audit trail.
- **(5) Cloud key-substitution MITM** — a malicious/compelled cloud could swap public keys at enrollment and become an undetectable middle → §5.3 now pins **TOFU + out-of-band verification codes** (Tailscale/Signal-style safety numbers) so key substitution is user-detectable, and manual pairing stays the trust anchor.

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

**One loopback connection per (peer, remote module)** (topology decided v4): the federation module opens one loopback connection **per exported remote module**, each carrying exactly one HELLO for `fed:<peer-fingerprint>:<module>`. This matches subc-core's semantics exactly (one registration per connection; `register_module_connection` evicts a prior registration on the SAME connection, so multi-HELLO-per-connection was never viable), preserves role fidelity (a remote ToolProvider re-exports as a ToolProvider with its real concurrency), scopes P1 updates per remote module, and gives connection-granular teardown per (peer, module). Per-peer isolation follows a fortiori. Catalog refresh within a live connection uses P1 (§2.6); the v2 evict-and-re-HELLO idea is withdrawn (re-gate: it kills in-flight calls on every remote module restart).

### 2.6 Two bounded subc-core primitives (v3 — the "zero subc-core change" premise dropped)
The re-gate proved federation cannot ride existing subc-core semantics alone. Rather than distort the module to fit (connection-churn on every catalog change; unsquattable namespaces by convention only), v3 adds two SMALL, generally-useful primitives to subc-core — both useful beyond federation:

- **P1 — `catalog.update` (non-disruptive provides-list refresh).** A channel-0 module-direction op letting an already-registered module replace **only the `provides` tools/ops list** of its manifest: registry entry updated, catalog generation bumped, existing route bindings untouched. Two normative constraints (v4, from the re-gate):
  - **No tool-granular GOODBYE.** Routes bind a module endpoint + channel; tool names live only in opaque bodies subc-core never parses. A call to a since-removed tool therefore reaches the module and gets a **module-side typed error** — identical to local behavior today (the module is authoritative for its own dispatch). P1 promises catalog VISIBILITY freshness, not route teardown.
  - **Frozen fields.** module_id, role kind, `concurrency`, and `control_ops` are HELLO-time properties (they size the live flow-control window and drive the health prober); a P1 payload that changes any of them is rejected with `catalog_update_frozen_field`. Changing them requires drain + re-HELLO, which is the correct cost for a semantics change.
  This is the incremental-catalog mechanism §8 Fork Cat anticipated, promoted from "future enhancement" to v1-blocking. Non-federation beneficiaries: any provider whose tool surface changes at runtime (AFT after an index build exposing tier-2 tools; subc-mcp reflecting an upstream provider change without reconnecting its shims).
- **P2 — namespace-prefix reservation.** Extend the reserved-module primitive from exact ids to id prefixes. Full semantics (v4):
  - **Syntax:** a reserved prefix MUST end with the `:` delimiter; an id matches iff `id.starts_with(prefix)` (so `fed:` never matches `fedx:tool`). **Exact-id reservations take precedence** over prefix reservations; overlapping prefix owners are rejected at config load. Boundary-case test matrix required.
  - **Ownership:** the config maps prefix → **owner supervised module_id**; a HELLO claiming an id under the prefix must present the OWNER module's current spawn nonce (not a nonce for the claimed id).
  - **Honest threat model:** P2 is **not a same-user barrier** — under subc's same-host model a same-user process can already read the connection key and the owner's env nonce, and is inside the trust floor. P2 protects against accidental id collisions and lower-trust/different-user processes, exactly like the existing exact-id reservation; the design claims no more. WHICH peer namespaces the fed-module creates under `fed:` is fed-module policy (profile-driven), not P2 enforcement.
  The federation module registers peer catalogs as `fed:<peer-pubkey-fingerprint>:<module>`. Builds on the shipped spawn-attestation machinery; the delta is delimiter-aware prefix matching + owner-module mapping.

Both primitives are independently testable, land before the federation module exists, and keep the thin-core rule honest: subc-core gains generic REGISTRY capabilities, zero federation logic.

---

## 3. Architecture

### 3.1 The federation module (the "trunk line")
A subc module, supervised like any other, present on each **full** node. It is **mandated `reserved` + launch-nonce-bound** (§5.2) so a rogue local key-holder cannot impersonate it. On each machine it is simultaneously:
- **A local provider (one loopback connection per (peer, remote module), §2.5)** that re-exports each peer's *exposed* catalog into the local catalog, namespaced by the **verified peer public key** (the human-readable name is display sugar; the authoritative namespace key is the pubkey, §5.2).
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
2. A learns B's **exposed** catalog (the subset B's `federation_exposure` permits) and re-registers it into A's local subc under the `fed:<B-pubkey-fingerprint>:` namespace via a per-peer provider HELLO (prefix-reserved, §2.6 P2).
3. Catalog changes on B propagate to A, which applies them in place via `catalog.update` (§2.6 P1) on the affected (peer, module) connection — in-flight routes are undisturbed; a call to a since-removed tool gets a **module-side typed error, never a route-GOODBYE** (§2.6 P1). Liveness/staleness is bounded and signed (§6.2).

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
Single-login convenience means the cloud distributes credentials, so it must not be able to impersonate. Mitigations baked in: the cloud distributes **public keys only** (data stays E2E, it can't decrypt), **device enrollment requires a device-held secret** (the cloud can't self-enroll a device), and **policy is signed** (a tampered network-map/ACL is detectable).

**Key-substitution MITM (v3/v4).** "Public keys only" does not by itself stop a malicious/compelled cloud from swapping DIRECTORY entries at enrollment and running an undetectable middle. The pinned ceremony (v4):
- **TOFU pinning:** a peer's pubkey, once learned, is pinned locally; the cloud can introduce a peer but never silently REPLACE a pinned key.
- **First contact is gated, not just detectable:** a cloud-introduced pair is **non-routable (deny-all exposure) until the out-of-band verification code is compared and confirmed** in the CK app on both ends. Friction lands exactly once, at introduction; zero after. The code is derived from **both endpoints' long-term device static keys** (never session/ephemeral material), so it binds the pair, not the conversation.
- **Rotation ceremony:** a legitimate key rotation must be **signed by the old key** (tombstone chain) or confirmed via an already-verified device / manual re-pairing; any other key change presents as first contact (non-routable + code comparison). "Changed key" is never a bare accept/reject prompt.
- **Residual, documented:** a cloud-tier user who skips the code comparison is trusting the cloud at introduction time — the accepted cost of the convenience tier. Manual pairing IS the out-of-band channel (the pairing token carries the key), so the self-hosted tier is structurally immune.

### 5.4 Remote-BindIdentity confused-deputy + the identity split (v3, re-gate finding 4)
Two DIFFERENT identities were conflated in v2; v3 separates them explicitly:
- **Caller identity (WHO is asking) = the authenticated peer pubkey** from the Noise session. It selects the `federation_exposure` policy set and is what the audit record names. It never appears in the local BindIdentity.
- **Execution identity (WHAT context the tool runs in) = profile-authored, local.** The serving machine's profile maps each peer to the local BindIdentity its calls run under (which project_root, harness, per-peer session scoping). The federation module stamps THAT on the local bind; a remote-supplied BindIdentity is never forwarded — otherwise a remote peer could steer a local tool into an arbitrary project.
- **Harness story (v4):** federated binds use a first-class `fed:<peer-fingerprint>` harness class. Providers validate harness against allowlists today (AFT: `{opencode, pi, runner, mcp:*}` — an unknown `fed:*` would be rejected at bind or run unscoped), so admitting the `fed:` class is a REQUIRED provider-side coordination item, with a defined config posture (fed-class binds get the same untrusted/project-tier cap as `mcp:*` unless the user's local config grants more). Verify against real AFT before phase 2.
Remote requests carry only tool + args + end-to-end correlation metadata. The audit record (§5.6) carries BOTH identities (peer pubkey → local identity used), so "which peer ran what, as what" is answerable after the fact.

### 5.5 Relay abuse / DoS
The relay does **auth-before-resource** (a connection proves device identity before any forwarding state is allocated) and enforces **per-account/per-device quotas + rate limits**, so an attacker can't amplify or exhaust the relay.

### 5.6 Cross-machine audit trail
Each cross-machine call emits a **tamper-evident local audit record** on the serving machine (which peer invoked which tool with what identity) — the central-risk action must be observable after the fact.

---

## 6. Reliability & correctness

### 6.1 End-to-end at-most-once across the hop (BLOCKER #3; mechanics v3, re-gate finding 3)
Local at-most-once (`NotSent` / `OutcomeUnknown`) **does not compose** across consumer→fed-module→[relay]→remote: local classification keys on local-socket "accepted", which collapses the remote leg's `NotSent` into the consumer's `OutcomeUnknown`, and there is no durable cross-machine send-log — so mutations can be lost-but-marked-unretryable, or auto-retried-and-duplicated.

Resolution — an explicit end-to-end state machine. The mechanics are standard WAL discipline (fsync intent before the first network write; fsync outcome before replying) and stand on their own:
- **Effect ID (v4):** `effect_id = (origin_device_pubkey, incarnation_uuid, seq)`. The incarnation UUID is minted whenever the origin's send-log db is created (fresh install, db loss, restore-from-backup) and stored IN that db — so a post-loss origin can never re-mint ids that collide with pre-loss effects. seq is monotonic within an incarnation; the serving side keeps a per-(pubkey, incarnation) high-water mark and REFUSES (typed fence error, never replay) a seq at-or-below it that isn't an exact dedup hit.
- **Origin send-log (cortexkit-store table in the fed-module's own db):** state machine per effect: `intent → sent → outcome(result|error|unknown)`. `intent` fsynced BEFORE the first network write; outcome fsynced BEFORE the local consumer sees the reply. NEVER auto-retries post-send on the WAN.
- **Serving-side dedup ledger:** records `effect_id → terminal outcome`. A re-arriving effect_id with a ledger row returns the RECORDED outcome without re-dispatch — what makes a legitimate origin re-send safe even when the first send actually arrived (lost-ack). **Retention (v4, co-defined):** a row is retained until the origin CONFIRMS outcome-received (piggybacked ack advancing a per-origin confirmed-watermark) plus a bounded grace; a post-expiry re-arrival gets a **typed ambiguity refusal** (`effect_outcome_expired`) — never re-dispatch, never a fabricated outcome. Residual: a manual re-send after both logs expire surfaces that refusal to the caller; documented, caller's decision.
- **Fed-state → CallError mapping (v4, closes the pre-intent-crash hole).** The origin consumer keeps the standard 4-variant taxonomy; the fed-module maps its durable state onto it, and RECOVERY reconciliation closes the one ambiguous window:
  | fed-module state at failure | origin consumer sees | safe next step |
  |---|---|---|
  | before intent fsync (incl. fed-module crash in the accept window) | `OutcomeUnknown` at the time — BUT on fed-module restart, recovery finds NO intent row, so the effect provably never left the machine; recovery emits a durable `not_sent` tombstone the consumer can query (or the caller simply re-invokes — no effect_id existed, nothing to duplicate) | re-invoke freely |
  | intent durable, send unconfirmed | `OutcomeUnknown`; recovery queries the SERVING ledger for the effect_id: ledger hit → settle with the recorded outcome; miss + peer reachable → provably not executed → `not_sent` tombstone; miss + peer unreachable → stays `unknown` until reachable | wait or surface |
  The tombstone/settlement store is keyed by **effect_id** and queryable via a fed-module management op (`fed.effect_status{effect_id} → not_sent | outcome(...) | unknown`); the origin consumer (or its harness) correlates via the effect_id the fed-module returned at accept time. This is the durable correlation key the phase-0 test vectors exercise.
  | outcome durable | terminal Response/Error as normal | done |
  Phase-0 test vectors must cover each row (crash-cut style, mirroring the existing real_daemon patterns).
- **Pure calls skip all of it** (re-send freely, no ledger row); `execution_mode` (§2.3) is the gate — the second axis doing its reliability job.
- **Fencing note:** the dedup ledger gives per-call at-most-once, not cross-call ordering. Order-sensitive remote mutation sequences remain the caller's problem (same as local subc today); documented, not solved.

### 6.2 Partition, liveness & catalog freshness (BLOCKER #4)
Re-exported catalog liveness + subc's silent-drop (no reactive NACK) means a cross-machine call during a staleness window can **vanish with no classification** (worse than `OutcomeUnknown`) or hang to timeout. Resolutions:
- **Per-peer keepalive** with a **bounded numeric staleness window**; on a missed keepalive the module marks the peer's re-exported tools unavailable rather than letting calls hang.
- **Partition settlement (v4 mechanism):** the fed-module's keepalive reaper is the AUTHORITATIVE partition classifier. On declaring a peer partitioned it **closes that peer's loopback connections**, so subc-core's connection-granular cleanup delivers deterministic route-GOODBYEs to every consumer of that peer's tools (`OutcomeUnknown` for in-flight). subc's module-direction GOODBYE alone is best-effort under backpressure by design and is never relied on for this.
- **Signed catalog/policy generations + tombstones**, with a **per-call policy-version check** so a stale exposure policy can't be exploited across a generation change.

### 6.3 Relay as product-critical infra
For the roaming case the relay is the **normal** path, not a fallback — an operational commitment (availability, capacity, geo-distribution). Real ops cost, flagged.

### 6.4 Device identity & revocation
Per-device keypairs in the control plane. Losing a phone → revoke that device's key from the cloud (and propagate a signed revocation/tombstone); other nodes are unaffected. `ClientHello` must carry a **device identity** so per-device keying/revocation is implementable (today it does not — a v2 transport addition for the federation/leaf path).

### 6.5 Cross-version federation
Two federated nodes may run different `subc-protocol` versions. Typed enums are closed (an unknown ProviderRole tag FAILS serde decode — it cannot be "skipped"), so the federation handshake exchanges **raw capability documents** (JSON), performs version/capability negotiation, and the fed-module filters/translates the raw doc down to the negotiated version BEFORE constructing the typed local manifest it hands subc-core via P1. Unknown roles/ops/fields are dropped at that raw layer; a newer peer can't break an older one.

---

## 7. Reference model

Closely analogous to **Tailscale**: a coordination server (identity, key distribution, ACLs, device mgmt) + a Noise-based data plane (WireGuard's core is Noise IK) with relay fallback, direct-when-possible. We deliberately take the **Noise IK security core** and our own thin relay, and **not** the IP-tunnel fabric (§2.2 scope decision). Our unit of access is a **tool/agent capability** (gated by `federation_exposure`), not raw IP connectivity.

---

## 8. Forks

**RESOLVED by this revision:**
- Exposure axis → dedicated `federation_exposure`, default deny-all (§2.3).
- Data-plane transport → Noise IK E2E (Fork T closed; mTLS-at-module is out, WireGuard deferred to a possible general-fabric future) (§2.2).
- Loopback-for-N-peers → one connection per (peer, remote module) (§2.5; refined from v2's per-peer by the v3 re-gate topology decision).

**STILL OPEN (for the re-gate + later calls):**
- **Fork 3 — NAT specifics.** Lean: reachable peer is server, NAT'd peer holds the outbound tunnel, relay when neither is reachable. Open: hole-punching vs always-relay for home-behind-NAT; keepalive/reconnect tuning for the held tunnel.
- **Fork 4 — profile schema depth.** §3.3 scope confirmed; open: versioning/reconciliation of cloud-distributed vs locally-paired profiles, and whether a profile also bundles per-environment model/auth/tool config.
- **Fork Cat — catalog-sync granularity.** RESOLVED mechanism: P1 `catalog.update` per (peer, module) connection (§2.6). Open: only the acceptable staleness-window number.
- **Fork C — cloud trust depth.** §5.3 mitigations stated; open: exact enrollment-secret mechanism and signed-policy key management.

---

## 9. Build phasing (de-risk smallest first)

0. **subc-core primitives first (§2.6):** P1 `catalog.update` (provides-list-only + frozen-field rejection) + P2 prefix reservation (delimiter semantics + boundary matrix + owner-module mapping), each with its own tests, landed and shipped BEFORE any federation module code exists. Phase-0 also carries the §6.1 test vectors (fed-state → CallError rows, crash-cut style) as executable specifications even though the fed-module itself comes later.
1. **Spike: two-peer direct federation, no cloud.** Two daemons on two reachable hosts (or two ports), federation-module pair, **Noise IK** session, manual key exchange, **one loopback connection per (peer, remote module), one HELLO each**, catalog re-export + a single cross-machine **exposed** tool call. Proves the module-pair shape, channel translation, the loopback-only invariant, and a confirming N-connections-one-process test. Reserved + nonce-bound module from the start; catalog refresh via P1 from the start.
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
- **Federation module = sole network endpoint; subc-core always loopback-only; one loopback connection per (peer, remote module)** — locked (§2.5, Ufuk; topology granularity refined v4).
- **Reserved + nonce-bound fed-module; namespace bound to peer pubkey** (§5.2); **end-to-end at-most-once w/ durable send-log + fencing** (§6.1); **keepalive + bounded staleness + GOODBYE-on-partition + signed generations** (§6.2); **local-injected BindIdentity** (§5.4); **relay auth-before-resource + quotas** (§5.5); **per-device keys + revocation** (§6.4); **cross-version negotiation** (§6.5) — drafted from council BLOCKERs.
- **v3 (re-gate findings):** "zero subc-core change" premise DROPPED → two bounded primitives **P1 `catalog.update`** + **P2 namespace-prefix reservation** (§2.6); at-most-once MECHANICS pinned (§6.1); caller-identity/execution-identity SPLIT (§5.4); cloud key-substitution MITM → TOFU + verification codes (§5.3).
- **v4 (v3 re-gate, 4/4 council):** P1 = provides-list-only, frozen fields rejected, NO tool-granular GOODBYE promise (module-side typed error for removed tools); P2 = delimiter-aware semantics + exact-over-prefix precedence + owner-module nonce mapping + HONEST same-user threat statement; §6.1 = incarnation epoch in effect_id + seq high-water fencing + recovery reconciliation + co-defined retention + fed-state→CallError mapping table; topology = one connection per (peer, remote module); partition classifier = fed-module reaper closing per-peer connections; cross-version = raw-doc filtering before typed decode; harness = first-class `fed:` class, AFT coordination required (phase-2 gate); TOFU = gated first contact + old-key-signed rotation + code binds long-term static keys. **Pending v4 re-gate; phase 0 gated on it.**
- **OPEN:** NAT specifics, profile-schema depth, catalog-sync granularity (narrowed: P1 IS the mechanism; open only the staleness number), cloud-trust depth (§8).
