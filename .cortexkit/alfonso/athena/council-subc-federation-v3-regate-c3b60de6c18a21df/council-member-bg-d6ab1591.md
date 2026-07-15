## Finding 1: P1 promises per-tool route behavior that the current wire model cannot express
- **Verdict**: BLOCKER
- **Location**: `docs/subc-federation-design.md:72-73,111`; `crates/subc-protocol/src/lib.rs:76-87`; `crates/subc-protocol/src/session.rs:45-53`; `crates/subc-protocol/src/manifest.rs:40-44`; `crates/subc-client-rs/src/consumer.rs:1665-1699`; `crates/subc-core/src/forwarding.rs:49-60,400-459`
- **Confidence**: high
- **Old v2 delta closure**: **Not closed**
- **New gap introduced**: v3 assumes subc can target “routes to tools that vanish,” but routes are provider-scoped, not tool-scoped.
- **Evidence**: P1 explicitly says removed tools get route-GOODBYE and cites AFT-style runtime tool-surface changes. But `RouteTarget::ToolProvider` carries only `module_id`, `route.bind` carries only that target, the client route cache is keyed by provider+identity (not tool name), and `RouteBinding` stores no tool/op identity.
- **Why it matters**: one ToolProvider route can serve many tools. If one tool disappears, subc cannot identify “the routes for that tool” without a protocol change. A provider-wide GOODBYE would also kill unrelated still-valid tools, defeating P1’s non-disruptive goal.
- **Concrete resolution**: narrow P1’s safe scope to additive/provider-level changes only, or explicitly accept provider-wide route teardown on ToolProvider catalog deltas, or redesign routing so binds are tool-specific (bigger protocol change).

## Finding 2: P1 does not define what manifest fields are mutable, but core behavior captures some at HELLO time
- **Verdict**: BLOCKER
- **Location**: `docs/subc-federation-design.md:72-73`; `crates/subc-core/src/control.rs:584-625,1248-1266,1752-1769,1852-1862`; `crates/subc-core/src/forwarding.rs:18-22,453-454,1277-1282`; `crates/subc-core/src/supervise.rs:1023-1045`
- **Confidence**: high
- **Old v2 delta closure**: **Not closed**
- **New gap introduced**: v3 says “replace manifest’s provided catalog in place,” but does not say whether concurrency/control ops may change.
- **Evidence**: `control_ops` are derived from HELLO and stored in the registry; health probing consults that stored set. Concurrency is read from the manifest at registration and used to size each route’s flow-control window.
- **Why it matters**: if `catalog.update` changes concurrency, old routes keep old windows while future routes may get different ones; if it changes advertised control ops, registry/prober state becomes stale or split-brain.
- **Concrete resolution**: make P1 reject any update that changes `module_id`, routable role shape, concurrency, or `control_ops`; or define a drain/re-register path for those fields instead of pretending they are in-place mutable.

## Finding 3: P2 names the right problem, but the authorization semantics are still underspecified
- **Verdict**: BLOCKER
- **Location**: `docs/subc-federation-design.md:67,73,110`; `crates/subc-core/src/supervise.rs:347-354,380-395,2023-2033`; `crates/subc-protocol/src/lib.rs:126-139`; `crates/subc-client-rs/src/lib.rs:521-532`; `crates/subc-core/src/forwarding.rs:168-174,280-282`; `crates/subc-core/src/registry.rs:38-44`
- **Confidence**: high
- **Old v2 delta closure**: **Not closed**
- **New gap introduced**: v3 says prefix registration is allowed only on a connection “owned by that module’s attested process,” but current machinery proves only bearer-nonce possession, and exact-vs-prefix collisions are unspecified.
- **Evidence**: today `reserved_hello_authorized` is exact-id lookup and returns `true` on miss. Spawn attestation is a single process nonce injected via env and echoed in HELLO. I found no daemon-side connection-owner field. Also the doc says both “one loopback connection per peer” and `fed:<peer>:<module>`, while same-connection re-registration currently evicts the old endpoint.
- **Why it matters**: the design does not answer precedence (`fed` vs `fed:`), delimiter rules (`fed:` vs `fedx:`), or how multiple fed-module connections are proven to be from the same attested instance. The one-connection-per-peer representation is also inconsistent with per-module namespaced ids unless clarified.
- **Concrete resolution**: specify delimiter-aware longest-match prefix rules; tag a connection with an owner identity after successful reserved/prefix HELLO; key future prefix authorization off that tag; and choose one topology explicitly: one synthetic module per peer, or one loopback connection per exported remote module.

## Finding 4: 6.1’s “accepted only after intent is durable” is incompatible with the current origin client boundary
- **Verdict**: BLOCKER
- **Location**: `docs/subc-federation-design.md:175-177`; `crates/subc-client-rs/src/consumer.rs:581-585,1174-1187,2219-2224`; `crates/subc-client-rs/tests/real_daemon.rs:294-303`
- **Confidence**: high
- **Old v2 delta closure**: **Not closed**
- **New gap introduced**: v3 relies on an acceptance point the plain subc client does not have.
- **Evidence**: the design says the fed-module reports accepted only after durable intent. But the current client classifies `OutcomeUnknown` once the writer path accepted the request, and `NotSent` only if the writer path/route-open failed before that. That boundary occurs before provider-level fsync. Tests confirm `OutcomeUnknown` is not auto-retried.
- **Why it matters**: if the writer accepts, then the fed-module crashes before durably recording intent, the origin still gets `OutcomeUnknown` and will not auto-retry — the exact lost-but-unretryable case v3 says it fixes.
- **Concrete resolution**: either add a protocol-level per-request accept ACK after durable intent, or move federation’s durable-send semantics into a federation-aware client API/wrapper instead of mapping them onto the current 4-variant client contract.

## Finding 5: 6.1 still lacks a sound replay-retention and effect-id durability contract
- **Verdict**: BLOCKER
- **Location**: `docs/subc-federation-design.md:175-179`; `crates/subc-client-rs/tests/real_daemon.rs:294-303`
- **Confidence**: high
- **Old v2 delta closure**: **Not closed**
- **New gap introduced**: the v3 ledger is bounded, but the resend horizon and id uniqueness rules are not.
- **Evidence**: the design requires server dedup retention “≥ the origin’s max legitimate re-send horizon,” but never defines that horizon. It also defines `effect_id = (origin_device_pubkey, monotonic_seq)` with no installation epoch or collision story if the origin DB is lost/reset while the device key persists.
- **Why it matters**: after GC, a legitimate replay can duplicate a remote mutation; after origin state loss, a fresh call can collide with an old `effect_id` and receive stale deduped outcome.
- **Concrete resolution**: define an explicit replay lease/TTL and server GC contract; specify post-expiry behavior as “do not redispatch, surface ambiguity”; and strengthen `effect_id` with a durable installation epoch or high-entropy component.

## Finding 6: 5.4 fixes the deputy split in principle, but the example harness value conflicts with current provider vocabulary
- **Verdict**: SHOULD-FIX
- **Location**: `docs/subc-federation-design.md:157-159`; `crates/subc-protocol/src/lib.rs:42-48`; `crates/subc-mcp/src/main.rs:1149-1156,3880-3890`
- **Confidence**: medium
- **Old v2 delta closure**: **Partially closed**
- **New gap introduced**: `fed:<peer>` is not obviously a currently-valid harness family.
- **Evidence**: the design proposes a local BindIdentity with a `fed:<peer>` harness marker. `BindIdentity.harness` is just a string, but the MCP shim documents the current provider contract as `opencode|pi|runner|mcp:<client>` and only auto-prefixes bare tokens. A `fed:...` value would pass through unchanged.
- **Why it matters**: AFT-class providers may reject or misclassify federated binds even though the confused-deputy issue itself is fixed directionally by making BindIdentity local-authored.
- **Concrete resolution**: settle a provider-compatible harness namespace before rollout, or extend the provider allowlist/registration story as part of federation.

## Finding 7: 5.3 mitigates post-pin silent replacement, but first-contact and rotation are still underspecified
- **Verdict**: SHOULD-FIX
- **Location**: `docs/subc-federation-design.md:151-153`
- **Confidence**: high
- **Old v2 delta closure**: **Partially closed**
- **New gap introduced**: the mitigation is prompt-based, not yet a complete trust-state machine.
- **Evidence**: v3 adds TOFU pinning plus an out-of-band verification code derived from both devices’ keys, and changed keys become loud re-verification events. But the doc does not say first-contact traffic/exposure is blocked pending verification, and it gives no legit-rotation ceremony.
- **Why it matters**: a malicious cloud can still win the first contact if the pair is usable before verification; later, “changed key” looks the same for attack and legitimate reprovision.
- **Concrete resolution**: make cloud-introduced peers unverified/non-routable until code comparison (or explicit user override), define a rotation ceremony, and specify exactly what the verification code hashes.

## Finding 8: 6.5 needs a raw-manifest filtering story before typed decode
- **Verdict**: NOTE
- **Location**: `docs/subc-federation-design.md:193-195`; `crates/subc-protocol/src/manifest.rs:36-37`
- **Confidence**: high
- **Old v2 delta closure**: n/a
- **New gap introduced**: the design says unknown roles/ops are excluded from exposure, but current typed manifests reject unknown role tags at deserialize time.
- **Evidence**: `ProviderRole` is a closed enum; unknown role tags fail serde decode.
- **Why it matters**: older peers cannot “exclude unknown roles from exposure” unless negotiation/filtering happens on raw data before normal manifest decode.
- **Concrete resolution**: specify that federation handshakes exchange raw capability docs (or a translated compatibility form) first, then decode only the filtered subset.

## Summary
- **Counts**: 5 BLOCKER, 2 SHOULD-FIX, 1 NOTE.
- **Delta status**:
  - **P1**: **not closed**.
  - **P2**: **not closed**.
  - **6.1**: **not closed**.
  - **5.4**: **partially closed**.
  - **5.3**: **partially closed**.
- **Overall verdict**: **NO-GO**.
- **Why**: the v3 doc correctly identifies the v2 failure modes, but the actual subc-core/client semantics still defeat three key claims: P1 assumes tool-granular route control that the wire does not have; P2 assumes prefix/owner semantics that current spawn-attestation state does not define; and 6.1 assumes an origin “accepted” boundary that the plain subc client does not expose. Those are design-level blockers, not implementation nits.
- **Must-fix before phase 0**: (1) narrow or redesign P1 so its scope matches provider-scoped routing, (2) freeze or explicitly re-register HELLO-captured fields under P1, (3) specify P2 prefix precedence, delimiter rules, connection ownership, and topology.
- **Can fix during later phases**: 6.1 replay lease/effect-id durability, 5.4 harness vocabulary rollout, 5.3 first-contact/rotation UX+state machine, 6.5 raw-manifest compatibility filtering. Also note that 6.4 is still an acknowledged transport dependency: the current `ClientHello` carries only nonce+role (`crates/subc-transport/src/auth.rs:25-28`), while the design correctly says device identity must be added (`docs/subc-federation-design.md:190-192`).