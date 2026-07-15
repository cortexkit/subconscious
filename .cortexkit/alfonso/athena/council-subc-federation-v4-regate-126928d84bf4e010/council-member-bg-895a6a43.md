## Per-Finding Confirmation (13 v3 findings)

**#1: CLOSED** — 2.6 P1 lines 84-85 ("No tool-granular GOODBYE. Routes bind a module endpoint + channel; tool names live only in opaque bodies subc-core never parses. A call to a since-removed tool therefore reaches the module and gets a module-side typed error"). Source-verified: `ModuleRouteKey { endpoint, channel }` (forwarding.rs:43-46) and `RouteBinding` (forwarding.rs:49-60) carry no tool identity; module-direction GOODBYE is best-effort by design (forwarding.rs:68-93). The fold matches the real routing model. (NOTE: a stale contradictory statement survives at line 130 — see NEW-CONTRADICTIONS.)

**#2: CLOSED** — 2.6 P1 line 86 ("Frozen fields. module_id, role kind, `concurrency`, and `control_ops` are HELLO-time properties ... a P1 payload that changes any of them is rejected with `catalog_update_frozen_field`"). Source-verified: concurrency is captured at register time into `ModuleConnection` (forwarding.rs:300-304) and sizes the window (forwarding.rs:18-22); `control_ops` derived at HELLO (control.rs:584) and stored in registry (registry.rs:83), read by health prober (control.rs:1260). The freeze-and-reject mechanism correctly addresses both the flow-window and prober-split-brain risks.

**#3: CLOSED** — 2.6 P2 lines 88-91 (delimiter-aware `starts_with(prefix)` with `:` boundary, exact-over-prefix precedence, overlapping owners rejected at config load, prefix→owner module_id verified against owner's current spawn nonce, honest same-user threat statement). Source-verified: `reserved_hello_authorized` is exact-id HashMap returning `true` on miss (supervise.rs:384-395); nonce injected via `SUBC_LAUNCH_NONCE_ENV` (supervise.rs:2033). The fold correctly extends exact→prefix with the documented same-user floor caveat rather than claiming a hard barrier.

**#4: CLOSED** — 6.1 lines 200-206 (fed-state → CallError mapping table with three rows: before-intent-fsync → `OutcomeUnknown` + recovery `not_sent` tombstone; intent-durable-send-unconfirmed → recovery queries serving ledger; outcome-durable → terminal). The pre-intent-crash hole is explicitly closed via recovery reconciliation emitting a durable `not_sent` tombstone. Phase-0 test vectors mandated (line 206). The mapping is honest about the `OutcomeUnknown`-at-time vs recovery-settles-later distinction.

**#5: CLOSED** — 6.1 line 197 (`effect_id = (origin_device_pubkey, incarnation_uuid, seq)`; incarnation UUID minted on db creation and stored IN that db; serving side keeps per-(pubkey, incarnation) high-water mark and REFUSES seq at-or-below it). The incarnation-epoch mechanism directly closes the DB-loss collision: a post-loss origin mints a new incarnation UUID, so re-minted ids never collide with pre-loss effects, and the serving-side fence prevents replay of stale outcomes.

**#6: CLOSED** — 6.1 line 199 (retention co-defined: row retained until origin CONFIRMS outcome-received via piggybacked ack advancing a per-origin confirmed-watermark plus bounded grace; post-expiry re-arrival = typed `effect_outcome_expired` refusal, never re-dispatch, never fabricated outcome; residual documented). The circular definition is broken — retention is now anchored to the origin's confirmed-watermark, not to an undefined "max re-send horizon."

**#6a: CLOSED** — 6.1 line 196 ("The mechanics are standard WAL discipline (fsync intent before the first network write; fsync outcome before replying) and stand on their own"). The llm-runner "proven" reputation appeal is dropped; the mechanics are restated as self-justifying standard WAL discipline. No unverifiable external appeal remains in 6.1.

**#7: CLOSED** — 5.4 line 180 (federated binds use first-class `fed:<peer-fingerprint>` harness class; providers validate harness against allowlists today; admitting `fed:` class is a REQUIRED provider-side coordination item with defined config posture — fed-class binds get untrusted/project-tier cap as `mcp:*` unless local config grants more; "Verify against real AFT before phase 2"). The harness story is now first-class with an explicit phase-2 verification gate, not hand-waved. The residual (AFT is external, behavior unverified in-repo) is honestly flagged as a phase-2 gate rather than claimed resolved.

**#8: CLOSED** — 5.3 lines 170-174 (TOFU pinning; first contact gated non-routable until OOB code compared; code derived from both endpoints' long-term device static keys; rotation signed by old key OR re-verification identical to first contact; residual documented — cloud-tier user who skips code prompt is vulnerable, manual pairing structurally immune). All three sub-issues (first-contact window, rotation ambiguity, code binding) are addressed with concrete mechanisms. The code-binding is pinned normatively to long-term static keys (line 172).

**#9: CLOSED** — 6.2 line 213 (fed-module's keepalive reaper is the AUTHORITATIVE partition classifier; on declaring a peer partitioned it closes that peer's loopback connections, so subc-core's connection-granular cleanup delivers deterministic route-GOODBYEs; subc's module-direction GOODBYE stays best-effort and is never relied on). Source-verified: module-direction GOODBYE is best-effort under backpressure (forwarding.rs:68-93); connection-granular cleanup is the deterministic path. The overstatement is corrected — the reaper, not subc GOODBYE, is authoritative.

**#10: CLOSED** — 6.5 line 223 (closed enum unknown tag fails serde decode; handshake exchanges raw capability docs (JSON); fed-module filters/translates to negotiated version BEFORE constructing typed local manifest handed to subc-core via P1; unknown roles/ops/fields dropped at raw layer). Source-verified: `ProviderRole` is a closed enum, unknown tags fail serde decode (manifest.rs:36-37). The raw-doc-filtering-before-typed-decode mechanism correctly addresses the impossibility of "skipping" unknown roles at decode time.

**#11: CLOSED** — 2.5 line 79 + changelog line 12 (one loopback connection per (peer, remote module), one HELLO each; matches `register_module_connection` eviction semantics which evict prior registration on same connection — forwarding.rs:280-282; preserves role fidelity and per-module P1 updates). Source-verified: `register_module_connection` evicts prior endpoint for same connection_id (forwarding.rs:280-282). The topology is decided and grounded in the eviction semantics. (NOTE: stale "one connection per peer" phrasing survives at lines 102, 238, 267 — see NEW-CONTRADICTIONS.)

**#12: CLOSED** — 6.4 line 220 (`ClientHello` must carry a device identity; today it does not — a v2 transport addition for the federation/leaf path). The finding was a NOTE flagged phase-4+; v4 confirms it as phase-4+ (changelog line 17: "ClientHello device-identity confirmed phase-4+"). No phase-0 gate; correctly deferred.

**#13: CLOSED** —  Fork Cat line 243 ("RESOLVED mechanism: P1 `catalog.update` per (peer, module) connection (2.6). Open: only the acceptable staleness-window number"). The stale "coarse re-HELLO" language is gone; Fork Cat now correctly states P1 IS the mechanism with only the staleness number open.

---

## NEW-CONTRADICTIONS (b-class cross-section contradictions introduced or left by v4 edits)

### NC-1: 4.1 line 130 contradicts 2.6 P1 fold on removed-tool GOODBYE
- **4.1 line 130:** "Catalog changes on B propagate to A, which applies them in place via `catalog.update` (2.6 P1) — in-flight routes to unchanged tools are undisturbed; **removed tools get route-GOODBYE**."
- **2.6 line 85:** "A call to a since-removed tool therefore reaches the module and gets a **module-side typed error**" — explicitly "No tool-granular GOODBYE."
- **Severity:** High — this is the EXACT finding #1 the v4 fold claims to close, restated as still-true in 4.1. A reader implementing from 4.1 would believe subc-core delivers tool-granular route-GOODBYE on P1 removal, which the source proves it cannot (forwarding.rs:43-60, 68-93). This is a stale residue from the pre-v4 text that the v4 edit to 2.6 did not propagate to 4.1.
- **Fix:** 4.1 line 130 must read "removed tools get a module-side typed error (no route-GOODBYE; 2.6 P1)" — matching 2.6 line 85.

### NC-2: Topology phrasing — "one connection per peer" vs "one connection per (peer, remote module)"
- **2.5 line 79 + changelog line 12:** "one loopback connection per **(peer, remote module)**" (the v4-decided topology).
- **3.1 line 102:** "A local provider (**one loopback connection per peer**)"
- ** line 238:** "Loopback-for-N-peers → one connection per peer (2.5)"
- ** line 267:** "one loopback connection per peer — locked (2.5, Ufuk)"
- **Severity:** Medium — the 2.5 body and changelog correctly state the per-(peer, module) topology, but three other locations (3.1,   decision log) retain the old "per peer" phrasing. The decision log (line 267) is marked "locked (2.5, Ufuk)" yet contradicts 2.5's actual text. Not architecturally dangerous (the authoritative 2.5 body is correct) but creates implementer confusion about whether one peer gets one connection or N.
- **Fix:** Update lines 102, 238, 267 to "one loopback connection per (peer, remote module)" to match 2.5 line 79.

---

## WEAKENED-DECISIONS (c-class regressions)

None found. The v4 folds strengthen or clarify previously-locked decisions without weakening them:
- P1 re-scope (provides-only, frozen fields) is a tightening, not a weakening — it removes an undeliverable promise (tool-granular GOODBYE) and adds a hard rejection rule.
- P2 honest threat statement is a clarification, not a weakening — it states precisely what P2 protects against rather than overclaiming.
- Topology decision (per peer, remote module) is a refinement of "one connection per peer" that preserves the per-peer isolation invariant a fortiori.
- 6.1 incarnation epoch + recovery reconciliation + co-defined retention are all additions that close holes without relaxing existing guarantees.

---

## PHASE-0 VERDICT: GO

All 13 v3 findings are CLOSED in the v4 text with mechanisms, not mere prose restatements. The source-grounded folds (#1, #2, #3, #9, #10, #11) match the real subc-core source lines I verified (registry.rs:74-75, forwarding.rs:43-60/68-93/280-282/300-304, control.rs:584/619/1260, supervise.rs:384-395/2033, manifest.rs:36-37). The two NEW-CONTRADICTIONS (NC-1 4.1 line 130 stale "route-GOODBYE"; NC-2 stale "one connection per peer" at lines 102/238/267) are documentation-consistency defects, not architecture defects — the authoritative 2.5/2.6 body text is correct, and the contradictions are stale residues in downstream sections that a 4-line edit fixes. Neither blocks building P1 `catalog.update` (provides-list-only + frozen-field rejection) or P2 prefix reservation (delimiter semantics + boundary matrix + owner-module nonce mapping) in subc-core, because the implementer's authoritative spec (2.6) is correct and self-consistent. The phase-0 6.1 test vectors (fed-state → CallError rows, crash-cut style) are also well-defined enough to write as executable specs. Fix NC-1 and NC-2 as a documentation pass alongside phase-0 code, but they do not gate starting the work.