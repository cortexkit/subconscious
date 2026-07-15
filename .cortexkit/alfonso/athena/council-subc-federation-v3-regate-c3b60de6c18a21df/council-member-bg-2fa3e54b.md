## Finding 1: P1 `catalog.update` is unspecified for concurrency, `control_ops`, and removed-tool routes
- **Severity**: critical  
- **Location**: 2.6 P1; `crates/subc-core/src/control.rs:619-626`, `584-589`; `crates/subc-core/src/forwarding.rs:271-306`, `18-22`; `crates/subc-protocol/src/lib.rs:441-443`  
- **Confidence**: high  
- **Issue**: v3 withdraws evict-and-re-HELLO but does not define how P1 updates state that today is fixed at HELLO. `register_module_connection` sets `concurrency` from the manifest once (`control.rs:619-625`); the credit window is derived from that (`forwarding.rs:18-22`, default 32 vs 1024). `control_ops` are taken from HELLO (`control.rs:584-589`) and stored on `ModuleRegistration` (`registry.rs:83`). `catalog.list` exposes those `control_ops` (`control.rs:754`). If P1 replaces the manifest in the registry but leaves the forwarding `ModuleConnection` unchanged, a catalog that changes `ToolProvider` concurrency leaves **live flow-control inconsistent with the advertised catalog**. If P1 also bumps forwarding concurrency on live routes, **in-flight credits** may violate the new window with no transition spec. Separately, `RouteTarget::ToolProvider` is **module_id only** (no tool name in the route key); bindings store `module_id` (`forwarding.rs:54-55`). `handle_route_open` checks module-level role only (`control.rs:836-841`, `1780-1792`), not manifest tool names. So “removed tools get route-GOODBYE” cannot be implemented at the forwarding layer without parsing opaque bodies; stale routes to a live module can keep calling until the provider rejects in-body.  
- **Evidence**: HELLO path couples manifest → registry + forwarding; tool routing is per-`module_id`; no `catalog.update` exists in tree.  
- **Suggested Fix**: Normative P1 spec: (1) whether `concurrency` / `control_ops` may change via P1; if yes, define atomic credit reconciliation or forbid changes while routes exist; if no, reject P1 payloads that differ from HELLO-time values. (2) For removed tools: either document module-side rejection + catalog generation on `route.open`, or add a tool-aware invalidation mechanism (without body parsing at subc-core). (3) Single atomic transition: registry generation bump + optional selective drain before exposing new catalog.

**v2 delta (P1):** Closes “re-HELLO kills all in-flight calls on every catalog change” **only if** P1 is implemented as claimed; **residue + NEW gaps**: concurrency/`control_ops`/removed-tool semantics are still open — prior finding partially closed, implementation spec is not.

---

## Finding 2: P2 prefix reservation + “connection owned by attested process” is underspecified and conflicts with launch-nonce exposure
- **Severity**: critical  
- **Location**: 2.6 P2, 5.2; `crates/subc-core/src/supervise.rs:384-395`, `2026-2033`, `401-411`; `crates/subc-core/src/control.rs:556-568`  
- **Confidence**: high  
- **Issue**: Today `reserved_hello_authorized` is **exact** `module_id` → nonce map; miss ⇒ authorized (`supervise.rs:389-390`). P2 needs longest-prefix vs exact-id precedence (`fed` vs `fed:` vs `fedx:`), and whether an exact reserved id shadows a prefix. “Connection owned by that module’s attested process” is not keyed in subc-core: HELLO only checks nonce for **reserved exact id** (`control.rs:556-568`), not `connection_id` ↔ spawn record. The fed-module opens **N loopback connections** (one per peer); all share one supervised process and one `SUBC_LAUNCH_NONCE_ENV` (`supervise.rs:2033`). Any **local process that can read that env** (or the connection file) can present the same nonce on its own connection and HELLO-register `fed:<victim-peer>:…` unless P2 binds **connection_id** (or first HELLO on that socket) to the supervisor’s spawn nonce for the **federation module_id only**. `spawned_consumer_authorized` keys on consumer’s claimed `module_id` + nonce (`supervise.rs:401-411`), not on provider HELLO under a prefix. A co-resident attacker is not blocked by P2 as described.  
- **Evidence**: Prefix matching and per-connection ownership are absent; nonce is injected into child env (readable by same-user local attackers).  
- **Suggested Fix**: P2 design must specify: prefix match algorithm + exact-over-prefix rules; HELLO gate = `(module_id matches reserved prefix) ⇒ connection_id registered on first successful HELLO from spawn-attested fed-module AND nonce matches fed-module reserved entry`; reject prefix ids on other connections even with stolen nonce unless connection is the one bound at spawn. Consider not reusing env nonce across multiple outbound connections or use per-connection server-issued tokens.

**v2 delta (P2):** Identifies real exact-id squatting gap; v3 **does not close** it until prefix + **connection binding** semantics are specified — **residue**; **NEW gap**: stolen launch nonce + missing connection ownership.

---

## Finding 3: 6.1 “accepted after intent durable” misaligns with origin `CallError` taxonomy and client retry behavior
- **Severity**: high  
- **Location**: 6.1; `crates/subc-client-rs/src/consumer.rs:581-593`, `295-296`, `369-373`  
- **Confidence**: high  
- **Issue**: Origin consumer only has `NotSent` / `OutcomeUnknown` / `Module` / `SubscriptionBackpressure`. `NotSent` = not accepted by writer or `route.open` before body send (`consumer.rs:582-583`). If the fed-module delays “acceptance” to the origin until after WAN `intent` fsync, failures **before** that are `NotSent` (origin may retry per `consumer.rs:369-373` for `NotSent`). After intent is durable, the fed-module must forward bytes on the loopback route; that is **writer-path accepted** ⇒ ambiguity ⇒ **`OutcomeUnknown`** only (`real_daemon.rs:294-303`). That matches 6.1’s WAN `OutcomeUnknown` story **if** the fed-module is the origin’s direct subc peer. Risk: calling this “reports accepted” is misleading — the taxonomy has no “intent recorded” state; operators/agents must treat post-send ambiguity as **never auto-retry** while origin may still **legitimately** re-send mutators with the **same** `effect_id` (dedup ledger). 6.1 does not pin when the origin consumer’s `call()` returns relative to fed-module send-log barriers (response only after outcome fsync is stated; intermediate states are not).  
- **Evidence**: Four-variant enum; auto-retry on `NotSent` only.  
- **Suggested Fix**: Map explicit fed-module states → consumer-visible outcomes in 6.1: pre-intent failures = `NotSent`; post-intent without terminal response = `OutcomeUnknown`; document that origin **must not** auto-retry `OutcomeUnknown` but **may** issue a new call with same `effect_id` per federation policy. Add phase-0 test vectors mirroring `real_daemon.rs`.

**v2 delta (6.1):** Closes “no mechanics” at design level; **residue**: barrier ↔ `CallError` mapping and origin retry interaction; llm-runner precedent **unverified** in-repo.

---

## Finding 4: Dedup ledger retention vs `effect_id` seq reset after origin DB loss
- **Severity**: high  
- **Location**: 6.1 (`effect_id`, dedup ledger window)  
- **Confidence**: medium  
- **Issue**: Serving ledger returns cached outcomes for re-sent `effect_id` within a “bounded retention window ≥ origin’s max legitimate re-send horizon.” If origin send-log is wiped and `monotonic_seq` resets, **new** `(pubkey, low seq)` can collide with **evicted** ledger rows (if retention < attacker/operator confusion horizon) or with **still-recorded** rows (false replay of old outcome to new intent). No spec for seq persistence (WAL), crash recovery, or “unknown effect_id after seq gap ⇒ reject / fence / operator action.”  
- **Evidence**: 6.1 lines 175-177 only; no seq durability or collision rules in doc.  
- **Suggested Fix**: Persist seq in origin store with fsync; include generation/tombstone in `effect_id`; on serving side, if seq regresses below high-water mark for that pubkey, refuse or require explicit re-pairing; ledger retention ≥ max(WAN retry horizon, origin WAL replay window) with documented numbers.

**v2 delta (6.1):** **NEW gap** (not in v2 text).

---

## Finding 5: 5.4 `fed:<peer>` harness vs provider validation — no registration story
- **Severity**: high  
- **Location**: 5.4; `crates/subc-mcp/src/main.rs:1149-1157`; `docs/subc-principal.md:103-104`  
- **Confidence**: medium  
- **Issue**: Profile-authored `BindIdentity` with `harness: "fed:<peer>"` is plausible for storage/routing, but **subc-mcp** already normalizes/validates harness tokens (`opencode|pi|runner|mcp:<client>`); bare tokens get `mcp:` prefix (`main.rs:1155-1156`). Real AFT is documented to treat harness as cosmetic for **trust** (`subc-principal.md:103-104`), but route.bind still carries harness to modules; federation design does not say whether AFT/MC/llm-runner **partition keys** or policy treat unknown harness as `config_divergence` / forced-restrict. Cross-peer tools may silently land in wrong store partition or fail closed.  
- **Evidence**: MCP shim harness rules in-tree; federation doc asserts profile harness without module contract.  
- **Suggested Fix**: 5.4 add normative harness registry: reserved `fed:<fingerprint>` namespace, module behavior (accept + audit tag vs reject), and CK profile validation before stamp.

**v2 delta (5.4):** Closes confused-deputy **conceptually**; **residue**: execution harness interoperability with existing providers.

---

## Finding 6: 5.3 TOFU does not bound first-contact malicious cloud; verification code binding unspecified
- **Severity**: medium  
- **Location**: 5.3  
- **Confidence**: high  
- **Issue**: TOFU pinning helps **subsequent** substitution; at **first** cloud-mediated introduction, user has no pin — malicious directory can MITM until OOB compare. Doc says manual pairing is immune (token carries key) but does not define what the **safety number** hashes (both long-term keys? device keys? account id? session transcript?), when it must be shown (every new peer vs only cloud path), or how CK distinguishes **legitimate rotation** (new device key) from **attack substitution** (both are “key changed — verify”).  
- **Evidence**: 5.3 lines 151-153; no cryptographic binding spec.  
- **Suggested Fix**: Pin normative fingerprint (e.g. Noise static keys both ends); require OOB compare on cloud-first pair; rotation = signed tombstone chain from old key + user confirm; document irreducible first-contact trust for cloud tier.

**v2 delta (5.3):** Closes “no mitigation named”; **residue**: first-contact window + rotation UX; **NEW gap**: exact code binding.

---

## Finding 7: Internal doc contradiction — Fork Cat still says “coarse re-HELLO” while body adopts P1
- **Severity**: medium  
- **Location**: `docs/subc-federation-design.md`  Fork Cat (line 214) vs 2.5/4.1  
- **Confidence**: high  
- **Issue**:  states “v1 is coarse re-HELLO per peer” while v3 changelog and 2.6 promote P1 as v1-blocking. Re-gate readers can implement the wrong catalog path.  
- **Suggested Fix**: Update Fork Cat to “P1 is mechanism; open item = staleness window numeric only.”

---

## Finding 8: 6.2 partition / silent-drop — design direction sound but not tied to subc contracts
- **Severity**: medium  
- **Location**: 6.2; `handle_route_open` / route-GOODBYE paths in `control.rs`  
- **Confidence**: medium  
- **Issue**: GOODBYE-on-partition reuses local contracts (reasonable), but no spec for fed-module marking peer tools unavailable vs daemon registry still showing `fed:*` modules as Active — risk of `NotSent` vs hang vs silent drop mismatch across the extra hop.  
- **Suggested Fix**: On partition, P1 or module marks peer catalog stale + rejects new `route.open` with `target_unavailable`; align with keepalive window numbers in phase 3.

---

## Finding 9: One-connection-per-peer vs same-connection `register_module_connection` eviction — design is consistent if N sockets
- **Severity**: low  
- **Location**: 2.5; `forwarding.rs:280-282`; `control.rs:2776-2795`  
- **Confidence**: high  
- **Issue**: Multiple `module_id`s on **one** connection would evict forwarding state; design’s one socket per peer avoids this. Phase 1 “multi-registration-per-process” must mean **multi-connection**, not multi-HELLO one connection.  
- **Suggested Fix**: Clarify in 2.5/ phase 1 test: N UDS connections, one HELLO each.

---

## Per v2-re-gate delta verdict (concise)

| Delta | Closes prior finding? | Residue / new gaps |
|--------|------------------------|---------------------|
| **P1** | Partially (avoids full peer reconnect churn) | Concurrency, `control_ops`, removed-tool vs route binding, atomicity |
| **P2** | Partially (names squatting attack class) | Prefix rules, connection ownership, nonce exposure |
| **6.1** | Yes for “name the machinery” | `CallError` mapping, seq/ledger retention, external llm-runner proof |
| **5.4** | Yes for confused-deputy split | `fed:` harness provider contract |
| **5.3** | Partially (TOFU + codes named) | First-contact MITM, rotation vs attack, code definition |

---

## Summary

| Severity | Count |
|----------|-------|
| critical | 2 |
| high | 3 |
| medium | 3 |
| low | 1 |

**Overall verdict: GO-WITH-CHANGES** — architecture direction is sound and v3 correctly drops the “zero subc-core change” fiction, but **phase 0 must not start** until P1 and P2 have implementable specs that address concurrency/`control_ops`/tool removal and prefix+connection binding (Findings 1–2).

**Must-fix before phase 0 (P1/P2 implementation):**
1. P1 normative state machine (registry + forwarding + `control_ops` + concurrency + removed tools).  
2. P2 prefix algorithm + connection-to-spawn binding (not nonce-alone).  
3. Resolve  Fork Cat vs P1 contradiction.

**Can-fix during phase 0 / parallel to spike:**
- 6.1 `CallError` test matrix and `effect_id`/ledger retention numbers (Finding 3–4).  
- 5.4 harness registry (Finding 5).  
- 5.3 OOB code definition and first-contact UX (Finding 6).  
- 6.2 partition catalog staleness wiring (Finding 8).

**Justification:** Source-grounded subc-core behavior shows HELLO-time coupling of flow control and duplicate-`module_id` registry semantics; v3’s P1/P2 bullets are the right primitives but current text does not yet guarantee safe in-place catalog refresh or non-squattable `fed:*` registration against a local nonce thief. Shipping primitives without those specs risks baking incorrect registry/forwarding invariants into the first federation spike.