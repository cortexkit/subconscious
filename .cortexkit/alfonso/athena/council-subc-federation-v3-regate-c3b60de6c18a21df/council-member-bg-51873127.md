## Finding 1: P1 is not atomic with route.open, and tool-level GOODBYE is not implementable as stated
- **Verdict**: BLOCKER
- **Location**: 2.6 P1; `control.rs` route open; `forwarding.rs` route bindings.
- **Confidence**: high
- **Issue**: P1 conceptually addresses v2’s “duplicate HELLO / re-register kills routes” problem, but the v3 delta does **not** specify an atomic registry+forwarding transition. A route can validate against one manifest generation and then commit against a now-stale forwarding endpoint after `catalog.update` removed the role/tool. Also, subc routes to a `module_id`, not to a specific tool, so “routes to tools that vanish get route-GOODBYE” is not currently representable.
- **Evidence**:
  - P1 promises “registry entry updated, catalog generation bumped, existing route bindings untouched … removed tools get route-GOODBYE” (`docs/subc-federation-design.md:72`).
  - `handle_route_open` reads the registry and validates required role before forwarding reservation (`crates/subc-core/src/control.rs:814-842`, `:912-914`).
  - `commit_route` later commits using only forwarding endpoint state; it does not re-check registry generation/manifest (`crates/subc-core/src/forwarding.rs:400-459`).
  - `RouteTarget::ToolProvider` carries only `module_id`, not tool name (`crates/subc-protocol/src/lib.rs:76-79`), and `RouteBinding` records module/channel/endpoint, not tool identity (`crates/subc-core/src/forwarding.rs:49-60`).
- **Why it matters**: The claimed invariant “never leave a route bound to a tool the registry no longer knows” is not satisfied by the current mechanics. P1 can introduce stale accepted routes unless it is versioned/atomic.
- **Resolution**: Define P1 as a versioned transaction: update registry + forwarding under a single ordered lock or CAS on per-module catalog generation; have `begin_route` capture generation and `commit_route` verify it. If tool-level GOODBYE is required, route bindings must record target/tool identity, or the design must explicitly downgrade the claim to “removed tools fail at provider/application layer.”

## Finding 2: P1 does not define immutable vs mutable HELLO-time properties
- **Verdict**: BLOCKER
- **Location**: 2.6 P1; concurrency/control-op registration paths.
- **Confidence**: high
- **Issue**: `catalog.update` says it replaces a manifest in place, but concurrency and `control_ops` are captured at HELLO/registration time. Changing them via catalog update is either ignored, inconsistent, or unsafe for in-flight routes.
- **Evidence**:
  - `control_ops` are computed from HELLO and stored in the registry (`crates/subc-core/src/control.rs:584`; `crates/subc-core/src/registry.rs:78-83`).
  - Health probing reads `registration.control_ops` (`crates/subc-core/src/supervise.rs:1023-1030`).
  - Manifest concurrency is read at registration and passed to forwarding (`crates/subc-core/src/control.rs:619-625`).
  - Forwarding stores concurrency on `ModuleConnection` and creates per-route `ChannelFlow` windows from it (`crates/subc-core/src/forwarding.rs:169-174`, `:443-454`, `:1277-1282`).
- **Why it matters**: If a manifest changes `StatelessParallel` → `Serial`, existing routes may still have 1024 credits. If P1 does not update forwarding, future routes may still use the old window. If it does update forwarding, shrinking live windows with outstanding credits needs explicit semantics. `control_ops` cannot be changed through a manifest-only update at all.
- **Resolution**: P1 must reject changes to module id, protocol, trust tier, bindings, concurrency, and control ops unless it performs a drain/re-register. Limit P1 to catalog-surface changes, or define a separate disruptive “capability reconfigure” primitive.

## Finding 3: “One connection per peer” conflicts with `fed:<peer>:<module>` registration
- **Verdict**: BLOCKER
- **Location**: 2.5, 2.6 P2, 4.1.
- **Confidence**: high
- **Issue**: v3 says one loopback connection per peer, but also says peer catalogs are registered as `fed:<peer-pubkey-fingerprint>:<module>`. Current subc registration is one manifest / one `module_id` per routable registration; registering multiple module ids over one connection is not supported.
- **Evidence**:
  - Design claims “one loopback connection per peer” (`docs/subc-federation-design.md:67`).
  - P2 names exported ids as `fed:<peer-pubkey-fingerprint>:<module>` (`docs/subc-federation-design.md:73`; also `:110`).
  - `ModuleManifest` has one `module_id` (`crates/subc-protocol/src/manifest.rs:14-20`).
  - `register_module_connection` evicts prior endpoint for the same `connection_id` before installing a new one (`crates/subc-core/src/forwarding.rs:271-308`).
- **Why it matters**: P1/P2 cannot be built correctly until the catalog shape is fixed. Either a peer is one synthetic provider module, or every remote module needs its own loopback provider connection, or subc-core needs a larger multi-module-per-connection primitive.
- **Resolution**: Choose one: (1) one synthetic `fed:<peer>` module with namespaced tool names; (2) one loopback connection per remote module id; or (3) explicitly design multi-registration-per-connection as a third core primitive.

## Finding 4: P2 prefix reservation lacks collision and matching semantics
- **Verdict**: BLOCKER
- **Location**: 2.6 P2; `SupervisorHandle::reserved_hello_authorized`.
- **Confidence**: high
- **Issue**: P2 requires prefix matching, but current reserved-module auth is exact-id only and authorizes on miss. v3 does not define exact-vs-prefix precedence, delimiter rules, or overlapping owner conflicts.
- **Evidence**:
  - Current reserved nonce map is exact `module_id → nonce`, and no entry means authorized (`crates/subc-core/src/supervise.rs:344-395`).
  - P2 proposes reserving a prefix such as `fed:` for a named module (`docs/subc-federation-design.md:72-73`).
- **Why it matters**: Ambiguous matching can create bypasses or accidental denial: `fed` vs `fed:`, `fedx:`, exact `fed:abc` reserved to another owner, nested prefixes, etc.
- **Resolution**: Add a formal reservation table with canonical prefix syntax, delimiter requirements, owner module id, and deterministic conflict rules. Reject overlapping exact/prefix reservations with different owners at config load, or use longest-specific-match with tests for boundary cases.

## Finding 5: P2’s “connection owned by attested process” is only a bearer nonce unless strengthened
- **Verdict**: BLOCKER
- **Location**: 2.6 P2; launch nonce plumbing.
- **Confidence**: high
- **Issue**: v3 says squatting `fed:*` requires the fed-module launch nonce, but current HELLO only carries claimed manifest id plus nonce. It does not carry an owner module id, and the nonce is injected as an environment variable. For prefix-owned ids where claimed id is `fed:*` but owner process is e.g. `subc-federation`, current exact-id nonce lookup does not map cleanly.
- **Evidence**:
  - `ModuleHelloBody` has `manifest`, `protocol_ver`, optional `control_ops`, optional `launch_nonce`; no owner-process field (`crates/subc-protocol/src/lib.rs:126-139`).
  - Supervisor records spawn/reserved nonces under the configured module id and injects the nonce via environment (`crates/subc-core/src/supervise.rs:2021-2033`).
  - Exact reserved HELLO auth checks only `nonces.get(module_id)` (`crates/subc-core/src/supervise.rs:384-395`).
- **Why it matters**: P2 must answer what proves that a `fed:<peer>:...` connection is owned by the attested federation process, especially across N loopback connections. A bearer env nonce also needs an explicit threat model: inheritance/leakage to helpers or same-host inspection would let another local key-holder squat the prefix.
- **Resolution**: Prefix reservations should map prefix → owner supervised module id, and verifier should compare against the owner’s current spawn nonce, not the claimed `fed:*` id. Prefer per-connection capabilities or OS peer-credential binding where available; otherwise explicitly document nonce-as-bearer limitations and prevent child-env leakage.

## Finding 6: 6.1’s “accepted after intent durable” does not fit the existing client taxonomy
- **Verdict**: BLOCKER
- **Location**: 6.1 at-most-once mechanics; `subc-client-rs` errors.
- **Confidence**: high
- **Issue**: The design says the fed-module reports accepted only after intent is durable. Existing subc clients define acceptance at the writer/route path, not at provider-durable-intent time. A local consumer can get `OutcomeUnknown` after the request body was accepted by the writer even if the fed-module crashed before fsyncing intent.
- **Evidence**:
  - 6.1 states intent is fsynced before network write and “reports accepted … only after intent is durable” (`docs/subc-federation-design.md:175-177`).
  - `CallError::NotSent` vs `OutcomeUnknown` is based on writer-path acceptance / terminal response observation (`crates/subc-client-rs/src/consumer.rs:581-585`).
  - Real-daemon tests assert accepted mid-call becomes `OutcomeUnknown` and is not auto-retried (`crates/subc-client-rs/tests/real_daemon.rs:291-303`).
  - Module control has route-bind and health-check, not a per-request durable-accept ack (`crates/subc-protocol/src/session.rs:42-56`).
- **Why it matters**: The old v2 gap remains: crash after local write acceptance but before durable intent can become lost-but-unretryable at the origin consumer.
- **Resolution**: Add a real per-request durable-accept primitive, or require federation mutators to use an application-level submit/resume protocol where the caller supplies/stores an effect id before sending. Without that, do not claim existing `NotSent`/`OutcomeUnknown` semantics compose.

## Finding 7: Dedup retention and effect-id monotonicity are not soundly specified
- **Verdict**: BLOCKER
- **Location**: 6.1 serving-side dedup ledger.
- **Confidence**: high
- **Issue**: The retention rule is circular: “bounded retention window ≥ origin’s max legitimate re-send horizon” is not a protocol unless both sides negotiate/enforce that horizon. Also, `effect_id = (origin_device_pubkey, monotonic_seq)` collides if the origin loses/restores its DB and reuses sequence numbers under the same device key.
- **Evidence**:
  - Effect id is defined as pubkey + monotonic seq (`docs/subc-federation-design.md:175`).
  - Dedup retention is only described as a bounded window relative to origin resend horizon (`docs/subc-federation-design.md:177`).
- **Why it matters**: If the serving row is evicted and the origin legitimately resends, the mutation can execute twice. If seq resets, a new effect may be suppressed as an old one or vice versa.
- **Resolution**: Define a signed replay horizon and expired-effect behavior: after expiry, reject rather than re-dispatch. Include a durable origin epoch/incarnation UUID in `effect_id`, fsync sequence allocation with intent, and require device-key rotation or epoch reset protocol after DB loss.

## Finding 8: 5.4’s `fed:<peer>` harness marker may fail AFT-class harness allowlists
- **Verdict**: SHOULD-FIX
- **Location**: 5.4 identity split.
- **Confidence**: medium
- **Issue**: The identity split correctly prevents remote-supplied BindIdentity, but the proposed local harness marker `fed:<peer>` may be rejected by providers that validate harness values.
- **Evidence**:
  - 5.4 suggests profile-authored BindIdentity may include a `fed:<peer>` harness marker (`docs/subc-federation-design.md:155-159`).
  - `BindIdentity` includes free-form `harness` (`crates/subc-protocol/src/lib.rs:42-48`).
  - subc-mcp comments state providers validate harness against `opencode|pi|runner|mcp:<client>` and reject bare tokens (`crates/subc-mcp/src/main.rs:1149-1155`). AFT itself is not in this repo, so direct AFT verification is unavailable.
- **Why it matters**: Federation may fail at route.bind with `config_divergence`, or teams may broaden harness allowlists unsafely just to make federation work.
- **Resolution**: Add a harness-registration/capability story. Profiles should validate chosen BindIdentity against provider-declared accepted harness classes, or use an existing accepted class such as `mcp:federation:<peer>` with explicit provider policy.

## Finding 9: TOFU mitigates replacement, not malicious first contact or ambiguous rotation
- **Verdict**: SHOULD-FIX
- **Location**: 5.3 cloud control-plane trust.
- **Confidence**: high
- **Issue**: TOFU pins after first learn, but a malicious/compelled cloud at first contact can still substitute keys unless out-of-band verification is mandatory before exposure. Key rotation is also ambiguous: a legitimate rotation and an attack both appear as “changed key.”
- **Evidence**:
  - v3 says first learned key is pinned and changed keys trigger re-verification (`docs/subc-federation-design.md:151-153`).
  - Verification code is described only as derived from both devices’ keys (`docs/subc-federation-design.md:153`).
- **Why it matters**: Users may accept or skip first-contact verification; later they cannot distinguish real rotation from substitution without an authenticated rotation path.
- **Resolution**: Gate nontrivial `federation_exposure` until verified. Require rotations to be signed by the old key or confirmed through another verified device/manual pairing. Define exactly what the safety code binds: both long-term device keys, account/profile id, peer labels, and session transcript.

## Finding 10: 6.2 overstates deterministic GOODBYE-on-partition
- **Verdict**: SHOULD-FIX
- **Location**: 6.2 partition/liveness.
- **Confidence**: medium-high
- **Issue**: v3 says partition settles in-flight calls deterministically via route-GOODBYE / channel-gone. Core forwarding explicitly treats module-targeted GOODBYE as best-effort under backpressure.
- **Evidence**:
  - 6.2 promises GOODBYE-on-partition and deterministic settlement (`docs/subc-federation-design.md:181-185`).
  - Forwarding comments state module GOODBYE delivery failure is dropped, not connection-closing, and modules must bound stale bindings with their own TTL (`crates/subc-core/src/forwarding.rs:68-90`).
- **Why it matters**: A federation module relying solely on local GOODBYE may leak in-flight WAN state or fail to classify promptly under backpressure.
- **Resolution**: Make the fed-module’s own keepalive/deadline/reaper the authoritative partition classifier. Treat subc GOODBYE as a hint, not the sole deterministic mechanism.

## Finding 11: 6.5 compatibility requires manifest filtering before P1
- **Verdict**: SHOULD-FIX
- **Location**: 6.5 cross-version federation.
- **Confidence**: medium
- **Issue**: v3 says unknown roles/ops are excluded from exposure, but local `ModuleManifest` deserialization has a closed v1 role set. Passing a newer remote manifest through P1 directly will fail unless the federation module normalizes it first.
- **Evidence**:
  - Design promises unknown roles/ops excluded (`docs/subc-federation-design.md:193-195`).
  - `ProviderRole` is a closed enum for protocol v1; unknown role tags fail serde decode (`crates/subc-protocol/src/manifest.rs:34-39`).
- **Why it matters**: Cross-version negotiation must happen before constructing the local manifest sent to subc-core.
- **Resolution**: Specify that the federation module translates remote catalogs into the local protocol version, dropping unknown roles/fields and preserving tombstones/generations separately.

## Delta Closure Assessment
- **P1 `catalog.update`**: Does **not** close the v2 finding as written. It removes the duplicate-HELLO premise conceptually, but leaves atomicity, route staleness, tool-level removal, concurrency, and control-op semantics unresolved.
- **P2 prefix reservation**: Partially closes local non-fed squatting, but leaves prefix collision semantics and ownership/nonce binding unresolved.
- **6.1 at-most-once**: Does **not** fully close. The origin consumer taxonomy still lacks a durable-accepted state, and dedup retention/sequence durability are underspecified.
- **5.4 identity split**: Mostly closes remote-derived BindIdentity confused-deputy, but introduces a provider harness-allowlist integration gap.
- **5.3 TOFU**: Partially closes post-pin replacement, but leaves first-contact MITM, rotation disambiguation, and exact verification-code binding unresolved.

## Summary
Findings: **7 BLOCKER**, **4 SHOULD-FIX**, **0 NOTE**.

Overall verdict: **NO-GO** for proceeding to phase 0 as currently specified. The phase-0 primitives themselves, especially P1 and P2, are not yet precise enough to implement safely. Fix P1’s atomic/versioned update semantics, immutable HELLO-time fields, and catalog shape; fix P2’s prefix/owner/nonce semantics; then re-gate. The 6.1, 5.4, 5.3 issues can be scheduled after P1/P2 only if explicitly tracked, but they still block a safe federation release.