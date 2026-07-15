# subc Federation Design v3 — Adversarial Re-Gate Synthesis

**Intent:** AUDIT (adversarial re-gate)
**Council:** 4 valid responses — GPT 5.4 high, GPT 5.5 xhigh, XAI Composer 2.5, Ollama GLM 5.2. (Gemini Flash 3.5 completed but produced no usable response — emitted a malformed tool call instead of a review; excluded.)
**Question:** Verify v3 deltas (P1 `catalog.update`, P2 prefix reservation, §6.1 at-most-once, §5.4 identity split, §5.3 TOFU) close the v2 re-gate findings; hunt new gaps; per-finding BLOCKER/SHOULD-FIX/NOTE; end GO/GO-WITH-CHANGES/NO-GO for phase 0.

## Headline

The four members converge hard. **Every member independently found the same three BLOCKER-class gaps in the two phase-0 primitives (P1 and P2)** and grounded them in the same source lines. The architecture skeleton is judged sound and v3 is credited with correctly dropping the "zero subc-core change" fiction — but **all four agree phase 0 must not begin until P1 and P2 have implementable specs.** Two members label this **NO-GO**, two label it **GO-WITH-CHANGES**; the disagreement is purely nominal — both camps gate the same P1/P2 fixes before any code. Consolidated verdict: **GO-WITH-CHANGES, hard-gated on three must-fix items (effectively NO-GO until the P1/P2 specs land).**

The v2→v3 deltas are directionally right but under-close: **no delta fully closes its v2 finding**; three (P1, P2, 5.3) only partially close and introduce new gaps; two (6.1, 5.4) close the concept but leave an interoperability/durability residue.

---

## BLOCKER Findings (unanimous)

### #1: P1's "routes to vanished tools get GOODBYE" is architecturally impossible with the current routing model
- **Severity:** Critical — **Confidence:** Unanimous (4/4) — **Members:** GPT 5.4, GPT 5.5, XAI, GLM
- **Issue:** subc-core routes bind to a **module endpoint + channel, not to a tool.** `RouteTarget::ToolProvider` carries only `module_id` (subc-protocol/src/lib.rs:76-79); `ModuleRouteKey { endpoint, channel }` and `RouteBinding` carry no tool identity (forwarding.rs:43-46, 49-60). The tool name lives only in the opaque request body, which subc-core deliberately never parses (thin-core splice-without-parse invariant). Existing teardown (`remove_module_connection_locked` forwarding.rs:1123-1196; `begin_module_drain` forwarding.rs:744-801) operates at endpoint granularity — it releases ALL routes for the endpoint. There is no tool-granular route tracking and no tool-granular GOODBYE.
- **Impact:** P1's core promise ("routes to tools that vanish get the normal route-GOODBYE; everything else keeps flowing") cannot be delivered without either (a) tearing down ALL routes on ANY tool removal — reintroducing the exact disruption P1 exists to avoid, for the removed-tool case; or (b) leaving stale routes live so calls to a removed tool fail at the module with an opaque error and **no GOODBYE classification** — strictly worse than status quo for the consumer.
- **Fix direction (converged):** Re-scope P1 to ONE of: (1) `catalog.update` **adds tools only**; removals still require connection-level drain; (2) removals are allowed but the design **explicitly accepts** in-flight routes to removed tools get an opaque module-side error (not a GOODBYE) and documents this as the residual; or (3) add tool-granular route tracking to subc-core — a much larger change that breaks the thin-core invariant (members flag this as likely NO-GO). Pick (1) or (2) **before** phase 0.

### #2: P1 leaves concurrency and control_ops (captured at HELLO time) inconsistent on in-place manifest replacement
- **Severity:** Critical — **Confidence:** Unanimous (4/4) — **Members:** GPT 5.4, GPT 5.5, XAI, GLM
- **Issue:** P1 "replaces the manifest in place" without re-registering the connection, but two load-bearing properties are captured once at HELLO/register time:
  - **concurrency** → read via `manifest_concurrency` at register (control.rs:619-625), stored on the forwarding `ModuleConnection` (forwarding.rs:304), and it **directly sizes the per-channel request-credit window** (forwarding.rs:18-22: `DEFAULT_MODULE_MANAGED_WINDOW=32` vs `STATELESS_PARALLEL_WINDOW=1024`).
  - **control_ops** → derived from the HELLO (`effective_module_control_ops`, control.rs:584), stored in the registry (registry.rs:83), and **read by the health prober** (control.rs:1260) and exposed by `catalog.list` (control.rs:754).
- **Impact:** If P1 replaces the manifest but not the forwarding `ModuleConnection`: a concurrency change leaves the **live flow window inconsistent with the advertised catalog** (a module advertising StatelessParallel still throttled at 32), and a control_ops change leaves the **health prober reading stale ops** (split-brain). If P1 *does* touch live forwarding state, shrinking a window with **outstanding in-flight credits** has no defined transition and is unsafe.
- **Fix direction (converged):** P1 must state normatively that it updates **ONLY the `provides` tools list**; concurrency and control_ops remain HELLO-time properties whose change requires a full re-HELLO (connection drain). Alternatively, define an explicit atomic drain-then-reregister transition for those fields — which defeats "non-disruptive," so members recommend the freeze-and-reject approach: reject any P1 payload that changes module_id, role shape, concurrency, or control_ops.

### #3: P2 prefix reservation — semantics undefined AND "connection owned by attested process" is new machinery defeated by a same-user nonce reader
Two tightly-coupled sub-findings all four members raised; consolidated here.

- **Severity:** Critical — **Confidence:** Unanimous (4/4) — **Members:** GPT 5.4, GPT 5.5, XAI, GLM
- **Issue (semantics):** Today `reserved_hello_authorized` is an **exact `module_id` → nonce HashMap lookup that returns `true` on miss** (supervise.rs:384-395). P2 extends this to prefixes but does not define: exact-vs-prefix precedence (`fed` vs `fed:` vs the exact id `fed:peerA:tool`), delimiter/starts-with rules (does `fed:` match `fedx:tool`?), or longest-match behavior.
- **Issue (ownership):** P2 says a `fed:*` id is only accepted "on a connection owned by that module's attested process," but **subc-core has no connection-to-process binding.** Connections authenticate by symmetric key, not process identity; the only process binding is the launch nonce **presented in the HELLO body** (`ModuleHelloBody.launch_nonce`, lib.rs:126-139) and checked by exact-id lookup. The nonce is injected via `SUBC_LAUNCH_NONCE_ENV` (supervise.rs:2033) — **readable by any same-user process via `/proc/<pid>/environ`.** The fed-module opens **N per-peer loopback connections all sharing one process and one nonce**, so "ownership keyed by nonce" means any same-user key-holder who reads the nonce can HELLO-register `fed:<victim-peer>:...` and become the bridge — exactly the squatting P2 exists to stop. (`spawned_consumer_authorized` keys on consumer module_id + nonce, supervise.rs:401-411, and does not cover provider HELLO under a prefix.)
- **Impact:** As specified, P2 provides weaker protection than claimed: it is at best a **different-user** barrier, not the hard barrier the doc implies. Under the stated same-host-same-user-is-the-floor threat model this may be acceptable — but the design must **say so explicitly** rather than imply P2 is a hard wall. There is also an internal confused-deputy: the `fed:` prefix authorizes the fed-module for `fed:*`, but *which peer namespaces* it may create is fed-module policy, not something P2 enforces.
- **Fix direction (converged):**
  1. Specify canonical prefix syntax: delimiter-aware starts-with (`id.starts_with(prefix)` with `:` boundary), **exact-id reservations take precedence over prefix reservations**, longest-specific-match, reject overlapping owners at config load. Add a boundary-case test matrix.
  2. Map prefix → **owner supervised module_id**, and verify against that owner's current spawn nonce (not the claimed `fed:*` id).
  3. For real ownership: bind connection→process via **SO_PEERCRED / SCM_CREDENTIALS** on the loopback socket (verify connecting pid == supervisor-spawned pid), or issue **per-connection server-side tokens** instead of a shared env nonce. If same-user squatting is left in-scope of the accepted floor, **document that P2 is not a same-user barrier.**
  4. Clarify that per-peer-namespace authorization is enforced in the fed-module, not P2.

---

## §6.1 At-Most-Once — closes the concept, three durability/taxonomy residues

### #4: "Accepted-after-intent-durable" vs the origin consumer's 4-variant CallError taxonomy
- **Severity:** High — **Confidence:** Majority-with-nuance (4/4 examined; 3 flag misalignment, 1 finds it compatible-with-caveat)
- **Members:** GPT 5.4 (BLOCKER — no acceptance point exists), GPT 5.5 (BLOCKER), XAI (High — misaligns), GLM (NOTE — **compatible, but mapping must be documented**)
- **Issue:** The origin is a plain subc client with exactly `NotSent / OutcomeUnknown / Module / SubscriptionBackpressure` (consumer.rs:581-593). `NotSent` = writer-path/route.open failed before body send; `OutcomeUnknown` = accepted by writer, no terminal response; `OutcomeUnknown` is **never auto-retried** (real_daemon.rs:294-303). There is **no "intent durably recorded" state** in the taxonomy.
- **The disagreement, resolved:** GLM's careful reading (Finding 9) shows the mapping IS *safe* when the fed-module is the origin's direct subc peer: pre-intent failures surface as `NotSent`, post-intent WAN ambiguity as `OutcomeUnknown`, and the consumer's never-retry-on-`OutcomeUnknown` is precisely the property that makes the composition correct. **BUT** GPT 5.4/5.5 identify a real hole GLM's happy-path glosses: if the **writer accepts the body and then the fed-module crashes before fsyncing intent**, the origin still sees `OutcomeUnknown` (won't retry) even though nothing durable was recorded — the exact "lost-but-unretryable" case §6.1 claims to fix. The writer-accept boundary occurs *before* the fed-module's intent fsync, so "reports accepted only after intent durable" is not something the current client boundary can express.
- **Impact:** Calling it "reports accepted" is misleading; without an explicit map, a crash in the pre-intent-fsync window is silently lost-and-unretryable at the origin.
- **Fix direction (converged):** Either (a) add a protocol-level **per-request durable-accept ACK** after intent fsync, or (b) move federation's durable-send semantics into a **federation-aware client API/wrapper** rather than overloading the 4-variant contract. At minimum, add the explicit **fed-state → CallError mapping table** to §6.1 plus phase-0 test vectors mirroring real_daemon.rs, and document that the origin may legitimately re-send the same `effect_id` (dedup handles it) even though it must not auto-retry.

### #5: effect_id monotonic seq is not cross-restart durable; origin DB loss collides
- **Severity:** High — **Confidence:** Unanimous (4/4)
- **Issue:** `effect_id = (origin_device_pubkey, monotonic_seq)` (design:175). If the origin's send-log DB is lost/restored/rebuilt while the **device key persists**, the seq resets and re-mints effect_ids that collide with pre-loss ones. The serving dedup ledger (if it still holds those rows) returns the **OLD outcome for a NEW call — silently dropping a mutation** (worse than a duplicate, and catastrophic-DB-loss recovery is exactly when at-most-once matters most).
- **Fix direction (converged):** Add a durable **installation epoch / incarnation UUID** to the effect_id: `(pubkey, epoch, seq)`, incremented on DB loss/recovery so recovered origins never collide with pre-recovery ids; and/or persist the seq high-water mark in a separate fsynced location. On the serving side, a seq regressing below the per-pubkey high-water mark should refuse/fence rather than replay a stale outcome.

### #6: Dedup-ledger retention window is circularly defined
- **Severity:** High/Medium — **Confidence:** Unanimous (4/4)
- **Issue:** Retention is specified as "≥ the origin's max legitimate re-send horizon" (design:177) but that horizon is **never defined** — and since re-sends can be manual, it is effectively unbounded. An evicted ledger row + a legitimate late re-send = a **duplicate remote mutation** (the exact failure §6.1 exists to prevent).
- **Fix direction (converged):** Co-define the two windows instead of leaving them independent: retain ledger rows until the origin's send-log has advanced past that effect's terminal outcome plus a bounded grace period; define post-expiry behavior as **"do not re-dispatch; surface ambiguity,"** and document the residual (manual re-send after both logs evict may duplicate — the caller's problem), with a concrete number.

### #6a: llm-runner "proven intent-log discipline" is an unverifiable external appeal
- **Severity:** Low/SHOULD-FIX — **Confidence:** Solo→corroborated (GPT 5.4 primary; XAI/GLM note "external/unverified")
- **Issue:** §6.1 grounds its reliability on llm-runner's "PROVEN" crash-cut-tested intent-log — but llm-runner is **not in this repo** (separate `cortexkit/llm-runner`), so the claim can't be source-verified here.
- **Fix direction:** The mechanics (fsync-intent-before-send, fsync-outcome-before-reply) are **standard WAL discipline and self-justifying** — restate them as such and drop the reputation appeal, or inline the crash-cut evidence.

---

## §5.4 Identity Split — closes confused-deputy, harness-registration residue

### #7: `fed:<peer>` harness marker has no provider-side registration/config story
- **Severity:** SHOULD-FIX — **Confidence:** Unanimous (4/4, medium)
- **Issue:** The identity split cleanly separates caller-pubkey (policy selection) from profile-authored local BindIdentity (execution context) — the confused-deputy is fixed directionally. But the example `fed:<peer>` harness marker is a value providers have never seen. The subc-mcp shim documents the accepted harness family as `opencode|pi|runner|mcp:<client>` and auto-prefixes bare tokens to `mcp:` (subc-mcp/main.rs:1149-1156); `fed:...` passes through unchanged. AFT uses harness as a config-cardinality key and rejects RootConfig-divergent harnesses at attach with `config_divergence` (subc-core-architecture.md:224), while treating harness as trust-cosmetic (subc-principal.md:103-104). AFT is an external repo, so behavior for an unknown `fed:*` harness is unverified.
- **Impact:** A `fed:<peer>` harness may (a) be **rejected at route.bind** (every federated call fails), or (b) be **silently accepted with no RootConfig** (tool runs unscoped/default — possibly unsafe), or (c) land tool state in the wrong store partition.
- **Fix direction (converged):** Settle a provider-compatible harness story before rollout — either a reserved `fed:<fingerprint>` namespace with a known default config template that the fed-module pre-provisions via the provider's management surface, or reuse an existing accepted class (e.g. `mcp:federation:<peer>`) carrying peer identity in the session field. **Verify against real AFT before phase 2.**

---

## §5.3 TOFU — makes substitution detectable, first-contact & rotation residues

### #8: First-contact window + rotation-vs-attack ambiguity + verification-code binding undefined
- **Severity:** SHOULD-FIX — **Confidence:** Unanimous (4/4)
- **Issue (first contact):** TOFU pins "once learned," but at first cloud-mediated introduction there is **no pin** — a malicious/compelled cloud can substitute at enrollment and TOFU pins the **attacker's** key. The out-of-band code is the mitigation, but the design's own UX premise (unsavvy user, "zero manual networking," §1.3) works *against* the user actually comparing codes, and nothing blocks first-contact traffic pending verification.
- **Issue (rotation):** A legitimate rotation and an attack-substitution present **identically** as "changed key." The design says a changed key is "a loud re-verification event, never auto-accept" but gives no mechanism to tell the two apart — the unsavvy user cannot distinguish "I didn't re-enroll" from "the cloud changed something."
- **Issue (code binding):** The doc says the code is "derived from both devices' keys" but never pins **exactly what it hashes** (long-term device static keys? account id? session transcript?).
- **Fix direction (converged):**
  1. Gate nontrivial `federation_exposure` as **unverified/non-routable until the OOB code is compared** for cloud-introduced pairs (friction only at introduction; zero after).
  2. Define a **rotation ceremony**: rotations must be signed by the old key (tombstone chain) and/or confirmed via another verified device/manual pairing, and re-verification on key change must require the **same OOB code comparison** as first contact — not a bare accept/reject.
  3. Pin the code to **both endpoints' long-term device static keys** (not session/ephemeral keys); state this normatively.
  4. Document the irreducible residual: a user who dismisses the first-contact code prompt on the cloud tier is vulnerable — this is the accepted cost of the convenience tier; manual pairing is the structurally-immune alternative.

---

## Lower-severity / corroborating findings

### #9: 6.2 GOODBYE-on-partition overstates determinism (SHOULD-FIX / NOTE)
- **Confidence:** Majority (3/4 — GPT 5.5, XAI, GLM)
- Core forwarding treats **module-targeted GOODBYE as best-effort under backpressure** (forwarding.rs:68-93) — deliberately never-close. So "settled deterministically via route-GOODBYE" is not guaranteed on the module side. GLM/GPT 5.4 note the composition *does* work if the fed-module **closes its own per-peer loopback connection** on keepalive loss, triggering connection-granular `cleanup_connection` → all routes for that peer GOODBYE → consumers see `OutcomeUnknown` (real_daemon.rs:801). **Fix:** make the fed-module's keepalive/deadline/reaper the authoritative partition classifier (subc GOODBYE is a hint), and document the close-per-peer-connection mechanism explicitly.

### #10: 6.5 cross-version — closed ProviderRole enum can't "exclude unknown roles" (NOTE)
- **Confidence:** Majority (3/4 — GPT 5.4, GPT 5.5, GLM)
- `ProviderRole` is a closed enum; unknown role tags **fail serde decode** (manifest.rs:36-37), so an older peer cannot "exclude unknown roles from exposure" — it can't parse the newer catalog at all. **Fix:** the federation handshake must exchange **raw capability docs** and have the fed-module filter/translate to the negotiated version **before** constructing the local manifest handed to subc-core via P1.

### #11: One-connection-per-peer vs register_module_connection eviction — consistent iff N sockets (NOTE / low)
- **Confidence:** Majority (3/4)
- `register_module_connection` evicts the prior endpoint for the same connection_id (forwarding.rs:280-282), so multiple `module_id`s over **one** connection would clobber each other. The design's "one loopback connection per peer" avoids this **only if** the phase-1 "multi-registration-per-process" test means **N UDS connections, one HELLO each** — not multi-HELLO on one connection. Members also flag that `fed:<peer>:<module>` (multiple modules per peer) needs a topology decision: one synthetic `fed:<peer>` module with namespaced tool names, OR one loopback connection per remote module. **Fix:** state the topology explicitly and clarify the phase-1 test.

### #12: 6.4 ClientHello device-identity addition correctly flagged, underspecified (NOTE)
- **Confidence:** Solo→corroborated (GPT 5.4, GPT 5.5)
- `ClientHello` carries only `client_nonce` + `role` (subc-transport auth.rs:25-28); adding device identity is a real transport change, accurately identified. Residue: whether it's a new `ClientHello` field (serde-default for back-compat) or a post-auth federation message, and how the asymmetric device key relates to the symmetric transport auth. **Phase-4+ concern, not a phase-0 blocker.**

### #13: Internal doc contradiction — Fork Cat still says "coarse re-HELLO" (NOTE)
- **Confidence:** Solo (XAI)
- §8 Fork Cat (line 214) still reads "v1 is coarse re-HELLO per peer" while the v3 body promotes P1 as the v1-blocking mechanism. **Fix:** update Fork Cat to "P1 IS the mechanism; open item = the staleness-window number only."

---

## Per-Delta Closure Verdict (consensus)

| Delta | Closes v2 finding? | Residue / new gaps |
|-------|--------------------|--------------------|
| **P1 catalog.update** | **Partially** — removes re-HELLO churn conceptually | Tool-granular GOODBYE impossible (#1, BLOCKER); concurrency/control_ops consistency (#2, BLOCKER); no atomic open→bind generation check (SHOULD-FIX) |
| **P2 prefix reservation** | **Partially** — names the squatting class | Prefix/collision semantics + connection-ownership + same-user nonce-bearer (#3, BLOCKER) |
| **§6.1 at-most-once** | **Yes (concept)** — mechanics now specified | Taxonomy mapping / pre-intent-crash hole (#4, High); effect_id seq durability (#5, High); retention circularity (#6, High); llm-runner unverified (#6a) |
| **§5.4 identity split** | **Yes** — confused-deputy cleanly split | `fed:<peer>` harness has no provider-config story (#7, SHOULD-FIX) |
| **§5.3 TOFU** | **Partially** — substitution now detectable | First-contact window, rotation ambiguity, code-binding undefined (#8, SHOULD-FIX) |

**No delta fully closes its v2 finding.**

---

## Summary Table

| # | Finding | Severity | Agreement | Members |
|---|---------|----------|-----------|---------|
| 1 | P1 tool-granular GOODBYE impossible (routes are module-scoped) | Critical | Unanimous | all 4 |
| 2 | P1 concurrency/control_ops HELLO-time inconsistency | Critical | Unanimous | all 4 |
| 3 | P2 prefix semantics + connection-ownership + same-user nonce-bearer | Critical | Unanimous | all 4 |
| 4 | §6.1 accepted-after-intent-durable vs 4-variant taxonomy / pre-intent crash | High | Majority (3 BLOCKER, 1 NOTE-compatible) | all 4 |
| 5 | effect_id seq not cross-restart durable (DB-loss collision) | High | Unanimous | all 4 |
| 6 | Dedup-ledger retention window circularly defined | High/Med | Unanimous | all 4 |
| 6a | llm-runner "proven" precedent unverifiable in-repo | Low | Minority | GPT 5.4, XAI, GLM |
| 7 | `fed:<peer>` harness has no provider-registration story | SHOULD-FIX | Unanimous | all 4 |
| 8 | TOFU first-contact / rotation ambiguity / code-binding | SHOULD-FIX | Unanimous | all 4 |
| 9 | §6.2 GOODBYE-on-partition overstates determinism | SHOULD-FIX | Majority | 3/4 |
| 10 | §6.5 closed ProviderRole enum can't exclude unknown roles | NOTE | Majority | 3/4 |
| 11 | one-conn-per-peer vs eviction; peer topology undecided | NOTE/Low | Majority | 3/4 |
| 12 | §6.4 ClientHello device-identity addition underspecified | NOTE | Minority | GPT 5.4, GPT 5.5 |
| 13 | Fork Cat still says "coarse re-HELLO" (doc contradiction) | NOTE | Solo | XAI |

---

## Priority Recommendations

### MUST-FIX before phase 0 begins (blocks building P1/P2)
1. **P1 removal semantics (#1):** choose add-only, or accept opaque-error-not-GOODBYE for removed-tool routes. Do NOT promise tool-granular GOODBYE — the routing model cannot deliver it without breaking the thin-core invariant.
2. **P1 field mutability (#2):** freeze concurrency + control_ops as HELLO-time; P1 updates ONLY the `provides` list; reject payloads that change frozen fields. Specify the atomic registry-generation bump.
3. **P2 spec (#3):** define prefix precedence/delimiter/longest-match + a boundary test matrix; map prefix→owner-module-id checked against the owner's spawn nonce; decide the connection-ownership mechanism (SO_PEERCRED/per-connection token) OR explicitly document P2 as a different-user-only barrier under the accepted floor.
4. **Doc fix (#13):** reconcile Fork Cat with P1 (cheap, do it now).
5. **Topology decision (#11):** state one-synthetic-module-per-peer vs one-connection-per-remote-module, and clarify the phase-1 "multi-registration" test = N connections.

### FIX DURING phase 0 / before the phases that consume them
6. **§6.1 taxonomy mapping + test vectors (#4)** — before phase 3; add the fed-state→CallError table and decide ACK-vs-wrapper for the pre-intent-crash hole.
7. **effect_id epoch + retention co-definition (#5, #6)** — before phase 3 (at-most-once build).
8. **Harness-registration story (#7)** — verify against real AFT before phase 2.
9. **TOFU first-contact gating + rotation ceremony + code binding (#8)** — before phase 6 (cloud), but pin the code-binding definition early.
10. **§6.2 partition mechanism (#9), §6.5 raw-manifest filtering (#10)** — before phases 3 and 1 respectively.
11. **Drop/soften the llm-runner "proven" appeal (#6a); §6.4 ClientHello field (#12)** — housekeeping / phase-4+.

---

## Consolidated Verdict: **GO-WITH-CHANGES, hard-gated (effectively NO-GO until P1/P2 specs land)**

The vote split 2 NO-GO (GPT 5.4, GPT 5.5) / 2 GO-WITH-CHANGES (XAI, GLM) is nominal only: **all four members gate the identical three P1/P2 must-fixes before any phase-0 code.** The architecture is sound and v3's engagement with the v2 findings is genuine — but the two phase-0 primitives, as written, would either fail to deliver their promised property (P1 tool-granular GOODBYE) or provide weaker protection than claimed (P2 same-user squatting), so building them now would bake incorrect registry/forwarding invariants into the federation foundation. **Resolve the three BLOCKERs (#1, #2, #3) in the design text, then re-gate the revised P1/P2 specs before writing code.** The §6.1/§5.4/§5.3 residues are real but can be scheduled to the phases that consume them, provided they are explicitly tracked.
