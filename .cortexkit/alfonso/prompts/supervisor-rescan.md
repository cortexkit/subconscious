# Task: `supervisor.rescan` — runtime module-set reconcile (hot add/remove of supervised modules)

## Why
Today the daemon reads `subc.jsonc` once at boot. `supervisor.reload` cycles an EXISTING module from its IN-MEMORY `ModuleSpec` — there is no way to add or remove a module key at runtime, and a changed `program` path in config is never picked up (deploys work only because we swap bytes at the same path). Product requirement: users install new plugin modules via the CK app (app writes config), and the module must come alive without restarting the daemon or disturbing the other running modules.

## What to build
A new channel-0 control op `supervisor.rescan` that re-reads the daemon config from disk and reconciles the supervised module set against it.

### Wire (subc-control)
- `ClientControlRequest::SupervisorRescan {}` (dotted op `supervisor.rescan`), following the existing enum/serde conventions exactly (look at `supervisor.reload` / `supervisor.list` variants).
- Response: `SupervisorRescanResult { added: Vec<String>, removed: Vec<String>, changed_pending_reload: Vec<String>, unchanged: u32 }` (module ids). Errors follow existing typed error-body conventions.
- subc-control is publish=false workspace-only, so this is NOT a crates.io wire event; still keep shapes additive and conventional.

### Semantics (subc-core)
1. **Config source**: re-read the SAME config file path the daemon booted with (bootstrap already resolves it — thread it to wherever the ControlHandler/Supervisor can reach it; store the resolved path at boot if not already stored).
2. **Fail-loud, no partial apply**: if the config fails to parse (JSONC → strict JSON → schema), or namespace-prefix reservations overlap (same validation as boot config-load), reject the WHOLE rescan with a typed error. Zero mutations in that case.
3. **Added module keys** (in new config, not currently supervised): run the full boot spawn path for each — `supervise_configured` semantics: mint + inject launch nonce (`SUBC_LAUNCH_NONCE`) and `SUBC_MODULE_ID`, track in the spawn-nonce map (universal spawn attestation), resolve + deliver storage descriptor config, register health probing per the module's config (on_degraded etc.), record disabled-but-configured modules in supervisor state without spawning (enabled=false ⇒ recorded, not spawned — same as boot).
4. **Removed module keys** (currently supervised, absent from new config): deliberate teardown — drain like the operator reload drain (module GOODBYE via the deliberate `send_module_goodbye` close path, route-gone propagation to clients as already happens on module death), stop the child, RETIRE the supervision task + snapshot row + spawn-nonce entry + health-prober registration. After rescan, `supervisor.list` must not show the removed id.
5. **Changed specs** (key present both sides but `ModuleSpec` fields differ — program, args, env, enabled, health config, storage): DO NOT restart the module. Update the stored in-memory spec so a subsequent `supervisor.reload` applies the new spec, and report the id under `changed_pending_reload`. EXCEPTION — `enabled` flips are applied via the existing set_enabled semantics (config disable ⇒ drain-stop; config enable ⇒ spawn), since enabled is state, not spec. Make sure `reload_child` actually uses the UPDATED spec after rescan (this fixes the latent stale-spec issue).
6. **Unchanged**: untouched — zero disturbance to healthy modules is the core invariant. A rescan that adds module X must cause NO observable event for module Y (no drain, no re-registration, no route churn). Write a test that asserts this explicitly (e.g. Y's connection stays up and a Y route call succeeds across the rescan).
7. **Concurrency**: rescan must be serialized against itself and against supervisor.reload/set_enabled/restart (take whatever supervisor-level ordering the existing ops use; if there's no cross-op serialization today, add a rescan-level mutex and document why). A second rescan while one is in flight gets a typed busy error or queues — pick the simpler one and test it.
8. **Watchdog/identity**: connection file, listener, storage roots for EXISTING modules are untouched. Rescan only reconciles the `modules` (+ their per-module storage/health) section. If TOP-LEVEL non-module config changed (port, storage backend default), report-only: include nothing in the diff lists, but log a warning that non-module config changes require a daemon restart. Do NOT attempt to apply those.

### CLI (ck)
`ck module rescan` — prints the diff report as a table (added / removed / changed-pending-reload / unchanged count), consistent with existing `ck module list` table style. Also add to `subc-probe` if trivial (`--supervisor-rescan`), skip if it drags.

### Tests (real-daemon integration, crates/subc-core/tests/, following existing patterns: level-triggered polling, subc-observable state, 10s setup timeout helper — NO sleeps-as-sync)
1. add-module: boot daemon with 1 module (fake-aft-stub), append a 2nd module key to the config file, rescan → report added=[m2], m2 becomes catalog-live + routable, launch nonce attested (route.open with consumer_identity resolves reserved principal).
2. remove-module: 2 modules → remove one from config → rescan → drained + gone from supervisor.list + its routes got GOODBYE + client's next route.open gets unknown_module. Other module undisturbed (assert its existing route still serves).
3. changed-pending-reload: change a module's args in config → rescan → reported, NOT restarted (same pid), then supervisor.reload → new spec applied (observable via changed arg effect or spawn env).
4. zero-disturbance: rescan that adds X while Y has an active in-flight request — Y's request completes normally, Y connection uninterrupted.
5. fail-loud: corrupt config → rescan rejected with typed error, module set unchanged; prefix-reservation overlap → same.
6. enabled-flip via config: enabled:false in new config → drain-stopped; back to true → respawned.
7. concurrency: two concurrent rescans → one applies, one gets busy/queued (whichever you implemented), no torn state.

### Invariants to preserve
- Thin-core: rescan supervises processes; it never parses module business config.
- Universal spawn attestation (every spawned module gets a nonce) must hold for rescan-added modules.
- The existing boot path and rescan path MUST share the spawn/validation code (extract shared fns if needed) — no duplicated spawn logic that can drift.
- All existing tests stay green: `cargo test --workspace`, clippy clean native AND `cargo clippy -p subc-core --target x86_64-pc-windows-gnu --all-targets` (Windows cfg gaps are a recurring CI failure class), `cargo fmt`.

## Verification bar
Full workspace test suite green + the 7 new integration tests + Windows cross-clippy. Commit with a clear message. Do NOT touch subc.jsonc in the repo root or any prod config.
