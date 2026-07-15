## Per-finding verdicts

**#1: PARTIAL** — 2.6:84–86, changelog:9 — P1 correctly rescopes to provides-list-only and module-side typed errors (matches `RouteBinding` / `ModuleRouteKey` at `forwarding.rs:43–60`, no tool in route keys), but **4.1:130 still says removed tools get `route-GOODBYE`**, contradicting the unanimous re-scope.

**#2: CLOSED** — 2.6:86, changelog:9 — Frozen `module_id`, role kind, `concurrency`, `control_ops` with `catalog_update_frozen_field` aligns with HELLO-time capture: `manifest_concurrency` + `register_module_connection(..., concurrency)` (`control.rs:619–625`, `forwarding.rs:276–304`) and `effective_module_control_ops` → registry `control_ops` (`control.rs:584–589`, `registry.rs:83`) used by health prober (`control.rs:1260`).

**#3: CLOSED** — 2.6:88–91, changelog:10 — Delimiter `:` + `starts_with`, exact-over-prefix, overlapping owners rejected at config load, owner-module nonce mapping, and explicit non–same-user barrier match v3 fix direction (SO_PEERCRED optional path not taken; honest threat model documented). Source: `reserved_hello_authorized` exact map, miss → true (`supervise.rs:384–395`); nonce via `SUBC_LAUNCH_NONCE_ENV` (`supervise.rs:2033`).

**#4: CLOSED** — 6.1:200–206, changelog:11 — Fed-state → CallError table plus recovery reconciliation (no intent row → provably `not_sent` / re-invoke) addresses pre-intent-crash hole; mapping is specified for phase-0 test vectors.

**#5: CLOSED** — 6.1:197, changelog:11 — `effect_id = (origin_device_pubkey, incarnation_uuid, seq)` with incarnation minted on DB (re)create and serving high-water / refuse regress closes post–DB-loss collision.

**#6: CLOSED** — 6.1:199, changelog:11 — Retention co-defined with origin send-log (confirm outcome-received + grace; post-expiry `effect_outcome_expired`, no re-dispatch) replaces circular “origin re-send horizon” definition.

**#6a: CLOSED** — 6.1:196, changelog:11 — v4 changelog drops llm-runner appeal; 6.1:196 states standard WAL discipline (residual historical mention remains only in v2→v3 changelog  not operative v4 normative text).

**#7: CLOSED** — 5.4:180, changelog:15 — First-class `fed:<peer-fingerprint>` harness, provider allowlist coordination, AFT verification before phase 2, config posture vs `mcp:*`; appropriately scoped as SHOULD-FIX / phase-2 gate, not P1/P2.

**#8: CLOSED** — 5.3:170–174, changelog:16 — Non-routable until OOB code compare, rotation via old-key chain or re-verification, code binds long-term device static keys, residual documented.

**#9: CLOSED** — 6.2:213, changelog:13 — Partition classifier = fed-module reaper **closing loopback connections** (connection-granular GOODBYE); module-direction GOODBYE best-effort only, consistent with `forwarding.rs:68–93`.

**#10: CLOSED** — 6.5:223, changelog:14 — Raw capability docs + negotiate + filter **before** typed manifest/P1; matches closed `ProviderRole` serde (`manifest.rs:36–37`).

**#11: CLOSED** — 2.5:79, changelog:12, phase 1 251 — Topology **one connection per (peer, remote module)**, one HELLO each; consistent with `register_module_connection` eviction on same `connection_id` (`forwarding.rs:280–282`).

**#12: CLOSED** — 6.4:220, changelog:17 — `ClientHello` device identity called out as transport addition, phase-4+; accurate underspec accepted as NOTE scope.

**#13: CLOSED** — 243, changelog:17 — Fork Cat: P1 is the mechanism; open item is staleness window only (no “coarse re-HELLO”).

---

## NEW-CONTRADICTIONS (v4 edits vs stale sections)

1. **Removed-tool teardown:** 2.6:85–86 / changelog:9 (module-side typed error, **no** tool-granular GOODBYE) vs **4.1:130** (“removed tools get **route-GOODBYE**”). This reintroduces the architecturally impossible P1 promise (#1).

2. **Loopback topology:** 2.5:79 / changelog:12 / 251 / v4 decision log:270 (**per (peer, remote module)**) vs **3.1:102** (“one loopback connection **per peer**”), **238** (“one connection per peer”), **267** (locked: “one loopback connection per peer”). Multi-module peers cannot be represented on a single connection without eviction (`forwarding.rs:280–282`).

3. **4.1:129** (“per-peer provider HELLO”) is loose wording vs normative **one HELLO per (peer, module)** on its own connection (2.5:79); ambiguous for implementers, not a full blocker if read with 2.5.

---

## WEAKENED-DECISIONS

- **None identified** for locked architecture skeleton (Noise IK, `federation_exposure`, loopback-only core, reserved fed-module, identity split). v4 **strengthens** P1/P2 honesty (same-user floor for P2) rather than weakening prior locks.
- **Caveat:** Leaving 267 and 238 as “per peer” **weakens enforceability** of the v4 topology decision (#11) in the decision log without changing the technical lock in 2.5:79 — documentation drift, not a semantic retreat.

---

## Summary

| Severity (if open) | Count |
|--------------------|-------|
| Blocker folds incomplete in doc | 1 partial (#1 — stale 4.1) |
| Cross-section contradictions | 2 major (GOODBYE, topology) |
| Weakened locked decisions | 0 |

**PHASE-0 VERDICT: NO-GO** — 2.6 P1/P2 normative text and source alignment close unanimous blockers #2 and #3 and substantially close #1 in 2.6, but the re-gate fails document integrity: **4.1:130** still mandates route-GOODBYE on tool removal (same failure mode as v3 #1), and **3.1:102 / 238 / 267** still lock “one connection per peer” against v4’s **per (peer, remote module)** topology required by `register_module_connection` eviction. Phase-0 implementation of `catalog.update` and prefix reservation should not start until those contradictions are edited to match 2.5–2.6 (estimated: lines 102, 130, 238, 267 + tighten 4.1:129); after that one doc pass, **GO** for subc-core P1+P2 with 6.1 mapping table as executable spec debt carried in phase 0 per 250.