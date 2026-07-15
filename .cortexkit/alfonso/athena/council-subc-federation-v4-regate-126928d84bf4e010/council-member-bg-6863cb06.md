#1: PARTIAL — 2.6 lines 84-85 (contradicted by 4.1 line 130) — The normative P1 text now matches the real routing model (`RouteTarget` only carries `module_id`, and `ModuleRouteKey`/`RouteBinding` are endpoint+channel only: `crates/subc-protocol/src/lib.rs:76-87`, `crates/subc-core/src/forwarding.rs:41-60`), but 4.1 still reintroduces the impossible “removed tools get route-GOODBYE” promise.

#2: CLOSED — 2.6 line 86 — Freezing `module_id`/role kind/`concurrency`/`control_ops` exactly matches current HELLO-time capture points (`crates/subc-core/src/control.rs:584-625,1260-1266`, `crates/subc-core/src/registry.rs:78-84`, `crates/subc-core/src/forwarding.rs:18-22,300-304`), so the v3 split-brain is closed.

#3: CLOSED — 2.6 lines 88-91 — v4 now specifies delimiter semantics, exact-over-prefix precedence, owner-module nonce mapping, and the honest “not a same-user barrier” statement, which is the right fold over the current exact-id/true-on-miss nonce gate and env-injected nonce model (`crates/subc-core/src/supervise.rs:384-395,2023-2033`; `crates/subc-protocol/src/lib.rs:96-100,126-139`).

#4: CLOSED — 6.1 lines 200-206 — The missing piece was the explicit fed-state→CallError mapping for the fixed 4-variant client taxonomy (`crates/subc-client-rs/src/consumer.rs:581-593`), and v4 now gives that table plus restart reconciliation for the pre-intent crash window.

#5: CLOSED — 6.1 line 197 — Adding `incarnation_uuid` to `effect_id` and fencing seq regression closes the DB-loss/restart collision the v3 re-gate identified.

#6: CLOSED — 6.1 line 199 — Retention is now co-defined with origin confirmation and post-expiry behavior is “typed ambiguity refusal, never re-dispatch,” which removes the earlier circularity.

#6a: CLOSED — 6.1 line 196 — v4 drops the unverifiable llm-runner appeal and grounds the design in ordinary WAL discipline instead.

#7: CLOSED — 5.4 line 180 — v4 now gives an actual provider-side harness story (`fed:<peer-fingerprint>` as a first-class class, explicit allowlist/config posture, and an AFT verification gate before phase 2) rather than leaving `fed:*` as an unexplained marker.

#8: CLOSED — 5.3 lines 170-174 — First contact is now explicitly non-routable pending OOB verification, rotation requires old-key proof or equivalent re-verification, and the code is bound to both endpoints’ long-term static keys.

#9: CLOSED — 6.2 line 213 — This now uses the correct mechanism: the fed-module closes the affected loopback connections, which makes route teardown ride connection cleanup rather than best-effort module GOODBYEs (`crates/subc-core/src/forwarding.rs:68-93,872-879,1183-1195`).

#10: CLOSED — 6.5 line 223 — v4 now correctly moves unknown-role handling to a raw-doc negotiation/filtering layer before typed decode, which is necessary because `ProviderRole` is closed and unknown tags fail serde decode (`crates/subc-protocol/src/manifest.rs:36-37`).

#11: PARTIAL — 2.5 line 79 (repeated correctly at  line 251) — The chosen topology now matches the real eviction semantics (`crates/subc-core/src/forwarding.rs:279-305`), but the doc still has stale one-per-peer wording in 3.1 line 102, 4.1 line 129,  line 238, and  line 267.

#12: NOT-CLOSED — 6.4 line 220 (and changelog line 17) — v4 still only says `ClientHello` “must carry a device identity” and explicitly punts it to phase-4+, so the transport-shape/back-compat residue remains.

#13: CLOSED —  line 243 (also  line 271) — Fork Cat no longer says “coarse re-HELLO”; it now states that P1 `catalog.update` is the mechanism and only the staleness number remains open.

## NEW-CONTRADICTIONS
- **Removed-tool semantics contradiction:** 2.6 lines 84-85 says removed tools get a module-side typed error and **not** tool-granular GOODBYE; 4.1 line 130 says removed tools get route-GOODBYE.
- **Topology contradiction A:** 2.5 line 79 says one loopback connection per **(peer, remote module)**; 3.1 line 102 says one loopback connection per **peer**.
- **Topology contradiction B:** 2.5 line 79 says per **(peer, remote module)**; 4.1 line 129 still describes re-export via a **per-peer provider HELLO**.
- **Topology contradiction C:** 2.5 line 79 says per **(peer, remote module)**;  line 238 says “one connection per peer.”
- **Topology contradiction D:** 2.5 line 79 says per **(peer, remote module)**;  line 267 says “one loopback connection per peer.”

## WEAKENED-DECISIONS
- None found; the regressions here are stale contradictory prose, not a quiet weakening of a locked decision.

PHASE-0 VERDICT: NO-GO

The P1/P2 core mechanics are now mostly specified well enough, but this is still not a clean executable spec for phase 0 because one of the unanimous phase-0 blockers (#1) remains only PARTIAL: 4.1 line 130 still re-promises the impossible removed-tool route-GOODBYE behavior that 2.6 correctly withdraws, and the topology decision behind P1 consumption is still contradicted in multiple live sections (#11). #12 is still open but is phase-4+ and not the gate here; the actual gate is that the phase-0 primitives must be described unambiguously before subc-core work starts. Fix the stale contradictory lines, then re-gate should flip to GO without reopening the architecture.