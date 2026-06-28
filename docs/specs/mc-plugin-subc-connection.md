# SPEC #4 — MC-plugin ↔ MC-subc connection

Status: **DRAFT for review.** Owner: Alfonso @ subconscious (subc-wire side) + MC (plugin-shim
side). Part of the MC-under-SUBC spec set (index: `docs/mc-subc-and-cache-foundations.md`).
Depends on: SPEC #1 (CK Message), the cache-policy core (`docs/cache-policy-core-design.md`),
SPEC #5 (MC module).

## 1. Purpose

The wire protocol between an OpenCode/Pi **plugin shim** (in the host process) and the
**MC subc module** (the daemon). This is the PLUGIN integration position: the harness drives
its own loop; each transform pass, the plugin hands the post-compaction message array to the
MC module over subc and gets the transformed array back to hand to the harness's serializer.

Contrast with MITM (SPEC #3): there we own the wire and forward with the harness's auth. Here
the harness serializes the final bytes from the transformed array we return — the same shape
as MC works today in-process, except the transform is now over-wire Rust instead of in-process
TS. **Byte-for-byte the same flow; only the transform's location moves.**

## 2. Roles

- **Plugin shim** (thin, in the host process; MC owns its internals): hooks the host's
  transform point, encodes the host message array → CK (the HARNESS-MODEL codec — MessageV2↔CK
  or AgentMessage↔CK, SPEC #2/MC), sends it to the MC module, receives the transformed CK,
  decodes CK → host messages, hands them to the host's serializer. Keeps out-of-band DELIVERY
  (Channel 1/2 nudges via the host's `tool.execute.after`/`sendMessage`, TUI, notifications) —
  those are NOT transform mutations (Edge E).
- **MC module** (the daemon's transform brain; SPEC #5): holds the frozen-set + durable store,
  runs the transform consuming the cache-policy core, serializes per-session.

## 3. The request (plugin → MC module)

A channel-data request on the plugin's route to the MC module:
```
{
  session_id,                 // stable per harness session; the MC per-session actor key
  serializer_profile,         // e.g. "opencode-aisdk-1.17" | "pi-0.80" — the healing-coverage key
  render_config,              // { system_hash, tool_set_id, model_key, serializer_profile_id } — the HARD-marker epoch inputs
  full_array_fingerprint,     // whole-input identity (DELTA staleness + LKG validity) — NOT the cache anchor
  payload,                    // one of: { full: CK[] } | { tail_delta: ... }   (see §4)
}
```
- `serializer_profile` (Edge C, REQUIRED): the MC module maps it to a healing-coverage table
  to compute the provider-quirk residual (`quirk_work = provider_requirement −
  serializer_healing(provider, profile)`). It's also what gates the reasoning-strip merge-term.
- `full_array_fingerprint` — **distinct from the cache anchor** (Oracle: the two were conflated).
  It is a fingerprint over the WHOLE input array (covered prefix AND tail), used for (i) DELTA
  staleness (does the daemon's held canonical still match the plugin's view) and (ii) LKG-replay
  validity (§7). The CACHE-REPLAY anchor itself is the cache-core's **boundary-presence** check
  (mechanism (a), `cache-policy-core-design.md`) — internal to the MC module over the array it
  holds, NOT a plugin-sent covered-prefix fingerprint. The plugin sends whole-array identity; the
  module owns boundary-presence.
- `render_config`: the single-unit render-config epoch (system+tools+model+serializer_profile_id);
  a change is a HARD bust.

## 4. always-full vs delta (Edge A — measurement-gated)

- **ALWAYS-FULL is the baseline** (required: MITM is always-full by nature; the plugin path
  reuses the same always-full daemon code). The plugin sends `{ full: CK[] }` each pass; the
  MC module holds the frozen-set and applies it. The full array is always cheap for the plugin
  to produce (the host re-hands it every pass today).
- **DELTA is a plugin-path OPTIMIZATION**, decided later from a measurement (a ~550-message /
  ~1MB subc round-trip at the target session size). If it justifies the complexity: the plugin
  sends `{ tail_delta }` + `full_array_fingerprint`; the MC module holds the canonical array and
  reconciles the delta. The daemon is authoritative on staleness — it returns **NEED_FULL_SYNC**
  when its held canonical doesn't match the plugin's `full_array_fingerprint` (cold daemon,
  any-position revert, or first pass), and the plugin re-sends `{ full: CK[] }`. So the plugin
  never guesses; up to 2 hops on a cold/revert pass, 1 hop steady-state.

**`full_array_fingerprint` (delta staleness) is NOT the cache anchor (boundary-presence)** —
different identities for different jobs (Oracle: the earlier `array_fingerprint == input_identity`
equation was a conflation). Delta staleness needs WHOLE-ARRAY freshness: a covered-prefix-only
anchor stays equal while the daemon's held TAIL goes stale (a new user message outside the covered
prefix), so a delta could apply to the wrong base without tripping NEED_FULL_SYNC. So NEED_FULL_SYNC
keys on `full_array_fingerprint`; cache replay keys on the module-internal boundary-presence.

## 5. The response (MC module → plugin)

```
{ transformed: CK[] }            // the transformed array (always-full path)
   | { transformed_tail_delta }  // delta path, if Edge A adopts it
   | NEED_FULL_SYNC              // delta path staleness sentinel → plugin resends full
```
The plugin decodes `transformed` CK → host messages and hands them to the host serializer. The
transform-time INJECTIONS (synthetic-todowrite, m0/m1 history, drop placeholders = frozen
units) are IN the returned array (Edge E: daemon owns injections). The MC module applies the
provider-quirk residual (the downstream gap-fill, SPEC #2 decide_*) to the returned array per
`serializer_profile` so the host serializer doesn't have to.

## 6. Durability boundary (Edge B)

- The MC module persists the cache-DECISION state (frozen_units + scheduler/boundary/overflow/
  historian-failure), **NOT message content** — content reconstructs from the plugin's
  full-array hand-off (the host DB is the source of truth for content).
- Restart: the held canonical is empty → next pass triggers NEED_FULL_SYNC (delta path) or just
  receives the full array (always-full path) → the MC module re-applies the durably-persisted
  frozen-set → byte-identical output.

## 7. Daemon-unavailable / transform-failure (Edge D)

- Fail-open raw-passthrough is **REJECTED** (causes double-bust / provider overflow).
- The plugin caches the LAST MC-returned transformed array per session (last-known-good). On
  daemon-down/timeout: if the current input's anchor still matches the last-known-good's
  coverage → re-emit the cached transformed array (safe, same bytes); if it diverged → **ABORT
  THE TURN** (surface a clean error), never a raw cache-busting/overflowing array.
- Health: the plugin reads subc Ping/Pong + connection-liveness (a real signal, not just an IPC
  timeout); a transform-failure returns a clean Error frame, not a hang. (The host-side
  quiet-cancel mechanism — OpenCode's duck-typed sentinel — is the plugin's concern; the subc
  side provides the clean error to trigger it.)

## 8. Per-session serialization (Edge F)

The transform is now async (await-subc); the host transform hook tolerates a promise. Rapid
consecutive passes for the SAME session must not race in the MC module — the MC module
serializes per-session via an in-process actor keyed by `session_id` (the subc coordinator
precedent; trivial vs today's cross-process leases).

## 9. Compaction marker (Edge G)

The host compaction marker (OpenCode's `filterCompacted` boundary) STAYS plugin-side: it's how
the plugin limits what it forwards (else it ships the daemon a pre-compaction 2M-token array).
The protocol operates on the POST-marker array; the marker-advance coordination with the host
stays plugin-side. The daemon never sees it. (On the owned/MITM paths there is no host marker —
the daemon/MITM module owns the boundary.)

## 10. What the plugin shim must NOT do

- NOT apply provider quirks itself (the MC module's downstream gap-fill does, per
  `serializer_profile`) — except where the host serializer already heals (then it's a no-op).
- NOT hold cache-decision state (the MC module owns the frozen-set).
- NOT push host config over the wire (config is the host's own on-disk read; the daemon is a
  read-only consumer).

## 11. Open items

- **Edge A** — the delta-vs-always-full decision (the subc round-trip measurement; Alfonso/subc
  owns, runs on request). Build always-full first; delta is additive.
- **Plugin-shim internals** (the host-side encode/decode + hook wiring) — MC owns; this spec
  fixes only the subc-wire contract. MC to fill the shim-side detail.
- **session_id derivation** per host (OpenCode vs Pi) — confirm the stable key.
