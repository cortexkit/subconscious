# v3 RE-GATE Audit — subc Federation Design

I read the full design doc (docs/subc-federation-design.md) and verified every source-grounding claim against the actual subc-core source. All orchestrator-provided facts checked out. Below are my findings, organized by the priority focus areas, with per-delta close/residue verdicts.

---

## Finding 1: P1 `catalog.update` — "routes to vanished tools get GOODBYE" is architecturally impossible with current routing

- **Verdict**: BLOCKER
- **Location**: docs/subc-federation-design.md:72, 111 (P1 spec); forwarding.rs:43-46, 188-189; lib.rs:76-79; forwarding.rs:1123-1196, 744-801
- **Confidence**: high
- **Issue**: P1 claims "routes to tools that vanish get the normal route-GOODBYE; everything else keeps flowing." But subc-core routes are bound to a **module endpoint + channel**, NOT to a tool. `RouteTarget::ToolProvider { module_id }` (lib.rs:77-79) targets the module; the tool name lives only in the opaque request body. `ModuleRouteKey { endpoint, channel }` (forwarding.rs:43-46) and `RouteBinding` (forwarding.rs:49-60) carry NO tool_id. The existing teardown machinery — `remove_module_connection_locked` (forwarding.rs:1123-1196) and `begin_module_drain` (forwarding.rs:744-801) — operates at **endpoint granularity**: it releases ALL routes for the endpoint, closes ALL flows, rejects ALL pending relays. There is no tool-granular route tracking and no tool-granular GOODBYE. P1 as specified cannot deliver "routes to vanished tools get GOODBYE while surviving tools keep flowing" without a fundamental routing-model change (tracking tool_id per route, which subc-core deliberately does not do — the thin-core "splice without parsing" invariant, subc-core-architecture.md:230).
- **Evidence**: forwarding.rs:43-46 (`ModuleRouteKey` has no tool field); lib.rs:77-79 (`RouteTarget::ToolProvider` carries only `module_id`); forwarding.rs:755-763 (drain closes ALL flows for endpoint); forwarding.rs:1183-1194 (remove releases ALL module_to_client routes for endpoint); control.rs:806 (`target_module_id` is the route key).
- **Why it matters**: This is the core premise of P1. If P1 cannot selectively tear down routes to removed tools, then either (a) catalog.update must tear down ALL routes on any tool removal (defeating the "non-disruptive" goal — same disruption as re-HELLO for the removed-tool case), or (b) routes to removed tools stay live and calls to them fail at the module (opaque error, no GOODBYE classification — worse than the status quo for the consumer). The design promises a property the routing model cannot provide.
- **Suggested Fix**: P1 must be re-scoped to one of: (1) catalog.update only ADDS tools (never removes) — removals still require connection-level drain; or (2) catalog.update removes tools but explicitly accepts that in-flight routes to removed tools get an opaque module-side error (not a GOODBYE), and the design documents this as the residual; or (3) P1 adds tool-granular route tracking to subc-core (a much larger change that breaks the thin-core splice-without-parse invariant — likely NO-GO). Option (1) or (2) must be chosen before phase 0.

## Finding 2: P1 `catalog.update` — concurrency/control_ops captured at register time; catalog.update leaves flow-control window and health-prober inconsistent

- **Verdict**: BLOCKER
- **Location**: docs/subc-federation-design.md:72 (P1 spec); control.rs:584, 619-625; forwarding.rs:18-22, 280-306; control.rs:1260; control.rs:1852-1862
- **Confidence**: high
- **Issue**: P1 proposes to "REPLACE its manifest's provided catalog in place" without re-registering the connection. But `concurrency` is read from the manifest at HELLO time (control.rs:619, `manifest_concurrency`) and passed into `forwarding.register_module_connection` (control.rs:620-625), where it sets the per-channel request-credit window (forwarding.rs:18-22: `DEFAULT_MODULE_MANAGED_WINDOW = 32`, `STATELESS_PARALLEL_WINDOW = 1024`). `control_ops` are likewise captured at HELLO time (control.rs:584, `effective_module_control_ops(hello.control_ops)`) and stored in the registry (registry.rs:83); the health prober reads `registration.control_ops` (control.rs:1260) to decide whether to probe. If P1's catalog.update replaces the manifest but does NOT re-register the connection, then: (a) a concurrency change in the new manifest does NOT change the live flow-control window (the `ModuleConnection.concurrency` in forwarding.rs:304 is stale) — catalog and flow window diverge; (b) a control_ops change does NOT update `registration.control_ops` — the health prober reads stale ops. If P1 DOES update these, it must touch the forwarding `ModuleConnection` (which is keyed by endpoint and whose concurrency is set once at register_module_connection), and changing concurrency on in-flight routes with outstanding credits is unsafe (the semaphore/window is already allocated).
- **Evidence**: control.rs:619 (`manifest_concurrency` read at register time); forwarding.rs:304 (`concurrency` stored in `ModuleConnection`); forwarding.rs:18-22 (window derived from concurrency); control.rs:1260 (health prober reads `registration.control_ops`); registry.rs:83 (control_ops stored at register).
- **Why it matters**: The design says P1 updates "registry entry + catalog generation" but is silent on concurrency/control_ops. Either the catalog becomes inconsistent with the live flow window (a module advertising StatelessParallel but still throttled at 32), or updating them requires touching live forwarding state with in-flight credits. This is a real safety/consistency gap.
- **Suggested Fix**: P1 spec must explicitly state: (1) whether catalog.update may change concurrency/control_ops; (2) if yes, the atomic transition (drain-then-reregister-connection is the only safe path for concurrency changes, which defeats "non-disruptive"); (3) recommended: P1 updates ONLY the `provides` catalog (tools list), and concurrency/control_ops remain HELLO-time properties — changing them requires a full re-HELLO (connection drain). Document this constraint explicitly.

## Finding 3: P1 — no atomic transition guarantees a route is never bound to a tool the registry no longer knows

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:72; control.rs:814-842 (route.open checks registry); forwarding.rs:355-398 (route bind)
- **Confidence**: medium
- **Issue**: P1 says "registry entry updated, catalog generation bumped, existing route bindings untouched." But `handle_route_open` (control.rs:814-842) checks the registry manifest's `provides` for the required role at route.open time. If catalog.update removes a tool between a route.open's registry check and the route.bind commit, a route could bind to a tool that was just removed. The design does not specify a generation-checked atomic transition (e.g., route.open snapshots the catalog generation and route.bind rejects if the generation changed). 6.2 mentions "per-call policy-version check" for policy but NOT for catalog generation on the bind path.
- **Evidence**: control.rs:836 (`target_has_required_role` checks manifest at open time); no generation check between open and bind in forwarding.rs:355-398.
- **Why it matters**: A race between catalog.update and route.open could bind a route to a just-removed tool. The window is small but real, and for federation (remote module restart → catalog.update) it's a routine event.
- **Suggested Fix**: P1 must specify that route.open captures the catalog generation and route.bind rejects with a retryable error if the generation has advanced past the open. Or: catalog.update marks the endpoint draining for removed-tool routes only (but see Finding 1 — tool-granular drain doesn't exist).

## Finding 4: P2 prefix reservation — "connection owned by that module's attested process" is entirely new machinery; nonce is a same-user-readable env var

- **Verdict**: BLOCKER
- **Location**: docs/subc-federation-design.md:73 (P2 spec); supervise.rs:384-395, 2033; auth.rs:25-28; server.rs:191-199
- **Confidence**: high
- **Issue**: P2 says "HELLO/registration of any id under that prefix is rejected unless it arrives on a connection owned by that module's attested process." But subc-core has NO connection-to-process binding. Connections are authenticated by symmetric key (server.rs:191-199, `authenticate_server`), not by process identity. The only process binding is the launch nonce, presented in the HELLO body (`ModuleHelloBody.launch_nonce`, lib.rs:138-139) and checked by `reserved_hello_authorized` (supervise.rs:384-395) — an exact-id HashMap lookup. The nonce is injected via `SUBC_LAUNCH_NONCE_ENV` (supervise.rs:2033), which on Linux is readable by any same-user process via `/proc/<pid>/environ`. The design's threat model accepts "same-host same-user key possession is the accepted floor" (subc-principal.md:12-13), so a same-user key-holder who can read the fed-module's nonce from `/proc` can present it in a HELLO and register any `fed:*` id. P2's "connection owned by" concept does not exist and is not built from existing machinery — it's a new authz layer.
- **Evidence**: supervise.rs:2033 (nonce via env); auth.rs:25-28 (ClientHello has no process binding); server.rs:191-199 (key auth only); supervise.rs:384-395 (nonce check is exact-id, body-presented); no `connection.*pid` or `process.*connection` binding exists (grep confirmed).
- **Why it matters**: P2's squatting protection is defeated by any same-user process that reads `/proc/<fed-module-pid>/environ`. The design claims P2 "builds directly on the shipped spawn-attestation machinery" but the "connection owned by process" part is NEW and the nonce-as-secret is weak against same-user readers. This may be acceptable under the stated threat model (same-user is the floor), but the design must say so explicitly rather than implying P2 provides a hard barrier.
- **Suggested Fix**: (1) Acknowledge that P2's nonce check is only a barrier against DIFFERENT-user processes, not same-user (consistent with the threat model). (2) If same-user squatting must be prevented, P2 needs a real connection-to-process binding (e.g., SCM_CREDENTIALS / SO_PEERCRED on the loopback socket to verify the connecting process's pid matches the supervisor-spawned pid) — specify this. (3) Clarify the prefix collision semantics (Finding 5).

## Finding 5: P2 prefix reservation — collision semantics between exact reserved ids and prefixes are unspecified

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:73; supervise.rs:384-395
- **Confidence**: high
- **Issue**: P2 extends reserved_nonces from exact ids to prefixes (e.g. reserve `fed:`). The current gate (supervise.rs:384-395) returns `true` (authorized) on exact-id MISS. Prefix matching introduces collision questions the design does not address: (a) Does reserving prefix `fed:` also cover the exact id `fed`? (b) Does `fed:` prefix match `fedx:tool`? (prefix vs starts-with semantics). (c) If both an exact reserved id `fed:peerA:tool` AND a prefix reservation `fed:` exist, which wins? (d) What stops the fed-module itself from registering `fed:<peerB>:tool` when policy meant a different module owns peerB — the prefix check only verifies the connection is the fed-module's, not that the fed-module is authorized for THAT specific peer namespace.
- **Evidence**: supervise.rs:389-390 (`match nonces.get(module_id) { None => true }` — exact lookup, miss = authorized).
- **Why it matters**: Ambiguous prefix semantics lead to either over-blocking (legit registrations rejected) or under-blocking (squatting permitted). Question (d) is the federation-internal confused-deputy: the fed-module is trusted for the `fed:` prefix, but which peer namespaces it may create is a federation-level policy, not a subc-core concern — the design must say this is enforced in the fed-module, not P2.
- **Suggested Fix**: Specify: prefix reservation uses starts-with (`id.starts_with(prefix + ":")`); exact-id reservations take precedence over prefix reservations; the fed-prefix authorizes the fed-module to register under `fed:*` but WHICH peer namespaces it creates is fed-module policy (P2 is namespace-squatting protection against OTHER local modules, not internal fed-module authorization). Add a test matrix for the collision cases.

## Finding 6: 6.1 — llm-runner intent-log precedent is external and unverifiable in this repo

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:174 ("borrowed from llm-runner's PROVEN intent-log discipline")
- **Confidence**: high
- **Issue**: The design grounds 6.1's fsync-barrier discipline on "llm-runner's PROVEN intent-log discipline (fsync intent before dispatch, fsync result before acting; memory of it: the durability contract that survived crash-cut testing)." llm-runner is NOT in this repo (confirmed: no `llm-runner/` directory; it's a separate repo `cortexkit/llm-runner`). The "proven" claim cannot be source-verified here. The design's reliability rests on an external, unverified precedent.
- **Evidence**: glob `**/llm-runner*/**` → 0 files; references are all in docs/ as external mentions (docs/subc-consumer-reconnect.md:40, docs/llm-runner-module-surface.md).
- **Why it matters**: If the intent-log discipline is not actually proven (or proven in a different context that doesn't transfer), 6.1's at-most-once guarantee is ungrounded. The design should stand on its own mechanics, not an appeal to an external repo's reputation.
- **Suggested Fix**: Either (a) inline the intent-log discipline's crash-cut test results into this design doc as evidence, or (b) restate 6.1's mechanics as self-justifying (fsync-before-send is a standard WAL discipline, not unique to llm-runner) and drop the "proven" appeal. The mechanics themselves (intent fsync before network write, outcome fsync before reply) are sound and standard; they don't need the precedent.

## Finding 7: 6.1 — serving-side dedup ledger retention window vs origin re-send horizon is a circular dependency

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:177 ("bounded retention window ≥ the origin's max legitimate re-send horizon")
- **Confidence**: medium
- **Issue**: The dedup ledger retention window must be ≥ the origin's max legitimate re-send horizon. But the origin's max re-send horizon is determined by how long the origin might wait before re-sending an `OutcomeUnknown` call — which is unbounded (the user might manually retry tomorrow). The design says the fed-module "NEVER auto-retries post-send on the WAN" (line 176), so re-sends are either manual or from origin-side crash recovery. If the origin's db is lost and restored from a backup, the monotonic seq resets (Finding 8), but even without that, a manual re-send after the ledger evicted the row would re-dispatch the mutation (duplicate side effect). The design does not define what "max legitimate re-send horizon" IS (a time? a seq distance?).
- **Evidence**: design 177 — no concrete retention number or horizon definition.
- **Why it matters**: An evicted ledger row + a legitimate late re-send = a duplicate mutation. This is the exact failure 6.1 exists to prevent.
- **Suggested Fix**: Define the retention window as a concrete function of the origin's send-log retention (e.g., ledger retains rows until the origin's send-log has advanced past that effect_id's outcome to `terminal` AND a bounded grace period has elapsed). Make the two retention windows co-defined, not independent. Document the residual: a manual re-send after both logs have evicted is a duplicate — acceptable if the window is large enough to cover all automated paths, with manual re-sends documented as "may duplicate, user's problem."

## Finding 8: 6.1 — monotonic seq in effect_id is not cross-restart durable; origin db loss resets seq and collides with prior effect_ids

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:175 ("effect_id = (origin_device_pubkey, monotonic_seq)")
- **Confidence**: high
- **Issue**: The effect_id uses a monotonic seq. If the origin's db (cortexkit-store table) is lost/corrupted and restored from a backup or rebuilt, the seq resets. A reset seq would mint effect_ids that collide with prior (pre-loss) effect_ids. The serving-side dedup ledger (if it still has those rows) would return the OLD outcome for a NEW call — a mutation is silently dropped. The design does not address seq durability across catastrophic origin db loss.
- **Evidence**: design 175 — seq is "monotonic" but no cross-restart durability mechanism specified (no epoch, no persisted high-water mark beyond the send-log itself).
- **Why it matters**: Catastrophic db loss is exactly when at-most-once matters most (recovery re-runs). A seq collision silently drops a mutation — worse than a duplicate.
- **Suggested Fix**: Either (a) persist the seq high-water mark in a separate durable location (e.g., a seq file fsynced on each mint) so a db restore doesn't reset it; or (b) include a per-origin epoch (incremented on db loss/recovery) in the effect_id: `(origin_device_pubkey, epoch, seq)`. Option (b) is cleaner — a recovered origin starts a new epoch and never collides with pre-recovery effect_ids (the ledger treats them as distinct, which is correct since the pre-recovery state is gone).

## Finding 9: 6.1 — "accepted-after-intent-durable" is compatible with the origin consumer's NotSent/OutcomeUnknown taxonomy, BUT the mapping must be explicit

- **Verdict**: NOTE (delta closes the prior finding, with a documentation residue)
- **Location**: docs/subc-federation-design.md:176; consumer.rs:581-593; real_daemon.rs:294-303, 421-422, 801
- **Confidence**: high
- **Issue**: The design says the fed-module reports `accepted` to the local consumer only after `intent` is durable, and an ambiguous WAN outcome surfaces as `OutcomeUnknown`. I verified the origin consumer's taxonomy (consumer.rs:581-593) is exactly `NotSent`/`OutcomeUnknown`/`Module`/`SubscriptionBackpressure`, and that `OutcomeUnknown` is never auto-retried (real_daemon.rs:294-303, route_retry only retries route.open pre-send per consumer.rs:987-1060). The mapping IS compatible: fed-module reports `accepted` (consumer sees route open + body accepted) → on WAN ambiguity, fed-module surfaces `OutcomeUnknown` → consumer never retries. This composes correctly BECAUSE the consumer's `OutcomeUnknown` semantics ("accepted but no terminal response") match the fed-module's "intent durable but WAN outcome unknown." The prior v2 finding (at-most-once was direction, not mechanics) IS closed by 6.1's state machine. Residue: the design does not explicitly state the mapping table (fed-module internal state → consumer-visible CallError variant), which should be documented.
- **Evidence**: consumer.rs:581-593 (4 variants); real_daemon.rs:294 (accepted→OutcomeUnknown), 303 (never retried); consumer.rs:987-1060 (retry is route.open only).
- **Suggested Fix**: Add a mapping table to 6.1: fed-module `intent-not-yet-durable + route.open fails` → `NotSent`; fed-module `intent-durable + WAN ambiguous` → `OutcomeUnknown`; fed-module `WAN terminal error` → `Module`; fed-module `WAN terminal success` → success bytes. State that the consumer's never-retry-on-OutcomeUnknown is the property that makes the composition safe.

## Finding 10: 5.4 identity split — `fed:<peer>` harness marker may be rejected by AFT's config-divergence check; no harness-registration story

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:158 ("a `fed:<peer>` harness marker"); subc-mcp/main.rs:1150-1155; subc-principal.md:103-104; subc-core-architecture.md:224
- **Confidence**: medium
- **Issue**: 5.4 says the profile maps each peer to a local BindIdentity with "a `fed:<peer>` harness marker." But AFT uses harness as a config-cardinality key: "a later harness whose RootConfig diverges is rejected at attach (`config_divergence`)" (subc-core-architecture.md:224). The subc-mcp shim auto-prefixes harness to `mcp:<client>` and the comment claims "Providers validate harness against `opencode|pi|runner|mcp:<client>`" (subc-mcp/main.rs:1150-1155). A `fed:<peer>` harness is a NEW value AFT has never seen. Whether AFT rejects it depends on AFT's RootConfig handling for unknown harnesses. subc-principal.md:103-104 says "harness stays cosmetic (routing/storage-slug); AFT audits that nothing trust-relevant keys off it" — suggesting AFT does NOT reject unknown harness values for trust, but the config-divergence check (subc-core-architecture.md:224) is about RootConfig divergence, not trust. A `fed:<peer>` harness with no RootConfig entry would either be ignored (no config for that harness) or trigger a divergence if it conflicts. The design has no harness-registration story — how does the `fed:<peer>` harness get a RootConfig entry so AFT doesn't reject or misconfigure it?
- **Evidence**: subc-core-architecture.md:224 (config_divergence on RootConfig divergence); subc-mcp/main.rs:1150-1155 (harness allowlist comment); subc-principal.md:103-104 (harness cosmetic); design 158 (fed:<peer> harness marker).
- **Why it matters**: If AFT rejects `fed:<peer>` harness at attach, every federated tool call fails at the bind. If AFT ignores it (no config), the tool runs with default/no config — possibly unsafe (no root-scoping). The design assumes the harness marker "just works" but doesn't specify the AFT-side config provisioning for it.
- **Suggested Fix**: Specify how the `fed:<peer>` harness is provisioned in AFT's config: either (a) the federation module pre-registers a RootConfig/SessionConfig entry for each `fed:<peer>` harness via AFT's management surface before the first call; or (b) AFT treats `fed:*` harnesses as a known class with a default config template; or (c) the harness marker is NOT `fed:<peer>` but reuses an existing harness value (e.g. `opencode`) with the peer identity carried only in the session field. Option (c) is the lowest-friction. Verify against the real AFT (external repo) before phase 0.

## Finding 11: 5.3 TOFU — first-contact window: a malicious cloud at FIRST contact (before any pin) is undetectable

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:152-153
- **Confidence**: high
- **Issue**: TOFU pins a peer's pubkey "once learned." But at FIRST contact, there is no pin yet — TOFU trusts the first key blindly. If the cloud is malicious at the moment of first introduction (enrollment), it can substitute its own key, and TOFU will pin the ATTACKER's key. The out-of-band verification code (safety number) is the mitigation for this — but the design says "cloud-introduced pairs get the code prompt" (line 153). The residual: the code prompt is only effective if the user ACTUALLY compares codes. If the user dismisses/ignores the prompt (the unsavvy-user case that is the explicit target use case — 1.3 "zero manual networking"), the first-contact substitution is undetected and TOFU pins the attacker. Manual pairing is "structurally immune" (the token carries the key), but cloud-introduced pairs depend on user diligence at a prompt the design's UX premise (zero-setup) discourages.
- **Evidence**: design 152-153 (TOFU + code prompt); design 30 (unsavvy user, zero setup).
- **Why it matters**: The cloud-convenience tier (the paid tier, 1.2) is exactly the tier where first-contact substitution is undetectable if the user skips the code check. The design's threat model (5.3) acknowledges the MITM but the mitigation's effectiveness depends on user behavior the UX premise works against.
- **Suggested Fix**: (1) For cloud-introduced pairs, make the code verification NON-optional for the first N calls (block tool calls until codes are compared) — friction at introduction, zero friction after. (2) Document the residual: a user who dismisses the code prompt at first contact is vulnerable to cloud substitution — this is the accepted cost of the convenience tier, and manual pairing is the zero-trust alternative. (3) Specify what the verification code binds EXACTLY: it should bind BOTH endpoints' long-term device keys (not the session key, not the directory entry) — the design says "derived from both devices' keys" (line 153) which is correct, but should state it binds the device identity keys, not ephemeral session keys.

## Finding 12: 5.3 TOFU — legitimate key rotation vs attack-substitution disambiguation is unspecified

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:152 ("a changed key is a loud re-verification event")
- **Confidence**: high
- **Issue**: Both a legitimate key rotation (user re-enrolls a device) and an attack-substitution (cloud swaps the key) present identically as "changed key." The design says a changed key is "a loud re-verification event in the CK app, never an auto-accept." But the design does not specify HOW the user tells them apart. Both trigger the same re-verification prompt. If the user expects a rotation (they just re-enrolled), they accept; if the user doesn't expect a rotation, they should reject. But for the unsavvy user, "I didn't re-enroll anything" vs "the cloud changed something" is not a distinction they can make. The design relies on the user knowing whether they initiated a key change.
- **Evidence**: design 152 (no rotation-vs-attack disambiguation mechanism).
- **Why it matters**: A compelled cloud could rotate keys (claim device re-enrollment) and the user, seeing an expected-looking prompt, accepts. The out-of-band code is the backstop, but only if re-verification also requires code comparison (not just "accept changed key").
- **Suggested Fix**: Re-verification on key change must require the SAME out-of-band code comparison as first contact, not just a "accept/reject changed key" prompt. A legitimate rotation requires the user to re-compare codes (the re-enrolled device shows its new key's code). This makes attack-substitution detectable at rotation, not just at first contact.

## Finding 13: 6.4 ClientHello device-identity transport addition is correctly identified but underspecified

- **Verdict**: NOTE (correctly identified, not a blocker for phase 0)
- **Location**: docs/subc-federation-design.md:191; auth.rs:25-28
- **Confidence**: high
- **Issue**: The design correctly identifies that `ClientHello` (auth.rs:25-28) carries only `client_nonce` and `role` — no device identity. Adding device identity is a real transport change. This is accurately stated. Residue: the design does not specify whether device identity is a new field in `ClientHello` or a post-auth federation-layer message, nor how it interacts with the symmetric-key auth prelude (the device key is asymmetric Noise, the transport auth is symmetric HMAC — two different key systems).
- **Evidence**: auth.rs:25-28 (`ClientHello { client_nonce, role }`); design 191.
- **Suggested Fix**: Specify the transport addition: a new optional `device_id` field in `ClientHello` (serde-default for backward compat), carried only on the federation/leaf path, validated by the federation module (not subc-core). This is a phase-4+ concern (relay/leaf), not phase 0.

## Finding 14: 6.2 partition classification — "GOODBYE-on-partition" reuses local contracts but the fed-module must synthesize GOODBYEs the partitioned peer cannot send

- **Verdict**: SHOULD-FIX
- **Location**: docs/subc-federation-design.md:184; forwarding.rs:68-93 (GOODBYE delivery semantics)
- **Confidence**: medium
- **Issue**: 6.2 says "in-flight cross-machine calls are settled deterministically (route-gone / OutcomeUnknown) on partition, reusing the local route-GOODBYE / channel-gone contracts." But on a partition, the remote peer CANNOT send a GOODBYE (it's partitioned). The LOCAL fed-module must SYNTHESIZE the GOODBYE/route-gone for the local consumer's in-flight routes to the partitioned peer. This is correct in principle but the design does not specify the mechanism: the local fed-module detects keepalive loss → marks the peer's re-exported tools unavailable → must tear down the local routes it created for that peer. But those routes are bound to the fed-module's per-peer loopback connection (the fed-module is the provider). The fed-module would need to close/drain its own per-peer loopback connection to trigger subc-core's route teardown — which is connection-granular (Finding 1), so ALL routes on that per-peer connection tear down (acceptable, since all routes on that connection go to the partitioned peer). This actually composes correctly, but the design should state it explicitly.
- **Evidence**: design 184; forwarding.rs:872-878 (cleanup_connection triggers remove_module_connection_locked).
- **Suggested Fix**: State explicitly: on partition detection (keepalive timeout), the fed-module closes the per-peer loopback connection, triggering subc-core's `cleanup_connection` → `remove_module_connection_locked` → all routes for that connection get GOODBYE → local consumers see `OutcomeUnknown` (real_daemon.rs:801). This is the correct composition; document it.

## Finding 15: 6.5 cross-version negotiation — "unknown roles/ops excluded from exposure" may silently drop a peer's entire tool surface

- **Verdict**: NOTE
- **Location**: docs/subc-federation-design.md:194
- **Confidence**: medium
- **Issue**: The design says "unknown roles/ops excluded from exposure so a newer peer can't break an older one." But if a newer peer's catalog uses a new ProviderRole variant (e.g. a future `AgentHost` role), the older peer's subc-protocol would fail to deserialize it (manifest.rs:36-37: "unknown role tags fail serde decode"). "Excluding from exposure" requires the older peer to tolerate the unknown variant at deserialize time — but the current protocol FAILS on unknown role tags (closed enum). So an older peer cannot even parse a newer peer's catalog, let alone exclude the unknown role. The design's "unknown fields tolerated" is inconsistent with the closed role enum.
- **Evidence**: manifest.rs:36-37 ("unknown role tags fail serde decode"); design 194 ("unknown roles/ops excluded").
- **Suggested Fix**: Either (a) the federation handshake negotiates a shared protocol version and the newer peer downgrades its catalog to the negotiated version's role set (the newer peer translates); or (b) the catalog exchange is federation-module-level (not raw subc-protocol manifest), so the fed-module can filter unknown roles before re-registering via P1. Option (b) is likely the intent (the fed-module is the bridge) — state it explicitly.

---

## Per-Delta Close/Residue Verdicts

### P1 `catalog.update` (2.6)
- **Does the delta close the prior v2 finding?** PARTIALLY. The prior finding was "coarse re-HELLO kills in-flight calls on every catalog change." P1 addresses the re-HELLO disruption BUT introduces NEW gaps (Findings 1, 2, 3): tool-granular GOODBYE is impossible with current routing; concurrency/control_ops consistency is unspecified; no atomic transition on the bind path. The delta does NOT fully close the finding — it changes the failure mode from "all calls die" to "routes to removed tools get no GOODBYE / catalog-flow-window divergence."
- **New gaps introduced**: Findings 1 (BLOCKER), 2 (BLOCKER), 3 (SHOULD-FIX).

### P2 namespace-prefix reservation (2.6)
- **Does the delta close the prior v2 finding?** PARTIALLY. The prior finding was "peer-namespace squatting by any local key-holder." P2 adds prefix reservation BUT the "connection owned by process" concept is new machinery (Finding 4), the nonce is same-user-readable (Finding 4), and prefix collision semantics are unspecified (Finding 5). Under the stated threat model (same-user is the floor), P2 closes the finding against DIFFERENT-user squatting but NOT against same-user squatting.
- **New gaps introduced**: Findings 4 (BLOCKER), 5 (SHOULD-FIX).

### 6.1 at-most-once mechanics
- **Does the delta close the prior v2 finding?** YES, with residues. The prior finding was "direction, not mechanics." 6.1 specifies the state machine, send-log, fsync barriers, dedup ledger. The taxonomy mapping (Finding 9) is compatible. Residues: llm-runner precedent unverified (Finding 6), retention-window circularity (Finding 7), seq durability on db loss (Finding 8).
- **New gaps introduced**: Findings 6 (SHOULD-FIX), 7 (SHOULD-FIX), 8 (SHOULD-FIX).

### 5.4 identity split
- **Does the delta close the prior v2 finding?** YES, with a residue. The prior finding was "caller-identity and target-selection conflated." 5.4 cleanly separates peer-pubkey (policy selection) from profile-authored local BindIdentity (execution context). Residue: the `fed:<peer>` harness marker has no AFT provisioning story (Finding 10).
- **New gaps introduced**: Finding 10 (SHOULD-FIX).

### 5.3 TOFU
- **Does the delta close the prior v2 finding?** PARTIALLY. The prior finding was "cloud key-substitution MITM undetectable." TOFU + verification codes make it DETECTABLE (not prevented) at first contact and rotation. Residues: first-contact window if user skips code (Finding 11), rotation-vs-attack disambiguation (Finding 12).
- **New gaps introduced**: Findings 11 (SHOULD-FIX), 12 (SHOULD-FIX).

---

## Summary

| Severity | Count |
|----------|-------|
| BLOCKER | 3 (Findings 1, 2, 4) |
| SHOULD-FIX | 8 (Findings 3, 5, 6, 7, 8, 10, 11, 12, 14) |
| NOTE | 3 (Findings 9, 13, 15) |

**Overall risk assessment**: The v3 deltas are directionally correct and show real engagement with the v2 findings. The architecture skeleton is sound. However, three BLOCKERs remain: P1's core promise (tool-granular GOODBYE) is architecturally impossible with subc-core's current routing model (routes bind to modules, not tools); P1's concurrency/control_ops consistency on in-place manifest replacement is unspecified and unsafe; P2's "connection owned by process" is new machinery that doesn't exist and whose nonce-secret is weak against same-user readers. These are not nitpicks — they are load-bearing premises of the two primitives the design proposes to build first.

## Overall Verdict: **NO-GO** for proceeding to phase 0 as specified

The two subc-core primitives P1 and P2 — the explicit phase-0 deliverables — both have BLOCKER-level specification gaps that mean building them as described would either fail to deliver the promised property (P1 tool-granular GOODBYE) or provide weaker protection than claimed (P2 same-user squatting). Proceeding to phase 0 with these gaps would build primitives that don't actually de-risk the federation module build.

**Path to GO-WITH-CHANGES**: Resolve the three BLOCKERs by re-scoping P1 and P2:
1. **P1 (Finding 1)**: Re-scope to "catalog.update adds tools; removals require connection-level drain" OR explicitly accept "routes to removed tools get opaque module errors, not GOODBYE." Pick one before phase 0.
2. **P1 (Finding 2)**: Specify that catalog.update changes ONLY the `provides` tools list; concurrency/control_ops remain HELLO-time and require full re-HELLO to change. Document this constraint.
3. **P2 (Finding 4)**: Acknowledge P2's nonce check is a different-user barrier only (consistent with the threat model); if same-user squatting must be prevented, specify a real connection-to-process binding (SO_PEERCRED/SCM_CREDENTIALS). Choose which before phase 0.

The SHOULD-FIX items (Findings 3, 5, 6, 7, 8, 10, 11, 12, 14) can be resolved during phase 0 / phase 1 — they are specification refinements, not primitive-design blockers. Finding 8 (seq durability) and Finding 7 (retention window) should be resolved before phase 3 (at-most-once build) but are not phase-0 blockers.