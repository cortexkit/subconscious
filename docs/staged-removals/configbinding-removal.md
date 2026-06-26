# Staged removal: `ConfigBinding` / `ConfigSource::SubcMediated` from the manifest

Status: STAGED (investigation done, not landed). Rides the next batched
`subc-protocol` crates.io republish. Author: subconscious (Alfonso), 2026-06.

## What and why

`subc_protocol::manifest` declares a config-DELIVERY capability that does not
exist:

- `Bindings.config: ConfigBinding { source: ConfigSource, tiers: Vec<String>,
  expansion: BTreeMap<String, Vec<TokenExpansion>> }`
- `ConfigSource` has exactly one variant, `SubcMediated`.
- The doc-comment claims *"subc stores tiered raw documents and transports literal
  token references; modules merge and expand them at use time."*

That capability was never built, and the config-unification work REMOVED the only
wire path that ever carried config (the `ConfigTier` field on `route.open` /
`route.bind`, deleted in `e0e7ce77`). The shipped pattern is
module-reads-its-own-config-home-file (`~/.config/cortexkit/<module>.jsonc` +
`<root>/.cortexkit/<module>.jsonc`), e.g. `subc-mcp`'s `read_gateway_config` for
`mcp.jsonc`. So `ConfigBinding` is dead, declared-but-unwired surface — its only
harm is misleading a module author into thinking subc delivers config.

Removal rationale: thin-core. subc is a router; it should not advertise a
config-delivery role it does not perform. Same logic that kept relayed config out
of `route.open`.

## Consumer surface (who DESERIALIZES the manifest)

`subc-core` is the ONLY deserializer of a module's HELLO `ModuleManifest`.
Verified: it has ZERO production reads of `manifest.bindings.config` — the only
references in `subc-core/src` are test code (`control.rs` test module,
`fake-aft-stub.rs`, `bench_harness.rs`). Nothing in the daemon path reads `tiers`
or `expansion` or delivers anything keyed off `source`.

## Producer surface (who CONSTRUCTS `config: ConfigBinding`)

Removing the struct field is a COMPILE-TIME break at every construction site (good
— no silent drift). Sites to update in lockstep:

- IN-REPO (subconscious): `subc-mcp/src/main.rs` `supervision_bindings()`,
  `subc-client-rs/examples/echo-module.rs`, `subc-core/src/bin/fake-aft-stub.rs`,
  `subc-core/src/bench_harness.rs`, the `control.rs` / `forwarding.rs` /
  `golden_json.rs` / `phase1_integration.rs` test builders, and the `manifest.rs`
  unit-test/`example_manifest` builder.
- TS CLIENT (subconscious): `clients/subc-client/src/provider.ts` — the
  `ConfigSource` / `TokenExpansion` type aliases, the `ConfigBindingInput`
  interface, `BindingsInput.config`, the two manifest builders (~lines 329, 923),
  `sortStringRecord`, and the `index.ts` re-exports.
- AFT (sibling repo, crates.io consumer): `aft/crates/aft/src/subc.rs:2325` —
  CONSTRUCT-ONLY. Verified AFT NEVER READS `bindings.config` (its only
  `ConfigBinding` references are the import at line 34 + the construction at 2325).
  So AFT just deletes the `config: ConfigBinding { .. }` block from its manifest
  builder when it bumps the protocol.
- OTHER MODULE REPOS (crates.io consumers, each builds its own manifest): the
  `ai-provider-quota`, `alfonso-routing`, `llm-runner` (`llmr-subc`), and
  `cortexkit-credentials` modules each have a `config: ConfigBinding { .. }` site
  in their manifest builder. Each deletes it on protocol bump.

## serde tolerance + SHIP ORDER (the load-bearing detail)

`ModuleManifest`/`Bindings` have NO `#[serde(deny_unknown_fields)]`, and `config`
is a REQUIRED field (no `#[serde(default)]`). That makes removal ASYMMETRIC across
a version skew between a deployed daemon and a connecting module (separate
processes, possibly built against different protocol versions):

- OLD module (still sends `config`) → NEW daemon (struct lacks `config`): serde
  IGNORES the unknown field (no `deny_unknown_fields`). ✓ SAFE.
- NEW module (omits `config`) → OLD daemon (struct still REQUIRES `config`): serde
  errors `missing field 'config'` → HELLO deserialization fails → module rejected.
  ✗ BROKEN.

Consequence: the DAEMON (consumer) must be at the new protocol version BEFORE any
module that dropped the field connects to it. A coordinated release (daemon +
modules bumped together, which is how the launchd daemon + its supervised modules
update on one machine) satisfies this. A rolling upgrade that brings a new module
up against a not-yet-updated daemon does NOT.

We are pre-release with NO backward-compat obligation (memory 6677), so the
recommended path is a CLEAN removal in one batched release (no Option-then-remove
two-step), shipped with the daemon and all modules in lockstep — matching the
clean-cutover preference. Do NOT land this as a standalone protocol republish; an
inert field does not justify its own AFT-coordinated release cycle.

## Exact removal (when the batch goes)

1. `subc-protocol/src/manifest.rs`: delete `pub config: ConfigBinding` from
   `Bindings`; delete `struct ConfigBinding`, `enum ConfigSource`, `enum
   TokenExpansion`; drop the now-unused `BTreeMap` import if nothing else needs it;
   update the `example_manifest`/golden builder.
2. Update every producer site above (all compile-time errors guide you).
3. `clients/subc-client`: delete the TS types/interface/builders/exports; update
   golden JSON.
4. Regenerate `subc-protocol/tests/golden_json.rs` golden bytes (the manifest wire
   shape changes — `bindings.config` disappears).
5. Republish chain (memory 7233): `subc-protocol` (minor bump) + `subc-transport`
   PAIRED re-pin, handed to AFT BEFORE publish so AFT bumps in one shot. Then AFT +
   the 4 module repos delete their construction sites against the new version.

## Coordination checklist (do at batch time, not now)

- [ ] Confirm AFT still only CONSTRUCTS (never reads) `bindings.config` at the time
      of the batch (re-grep `aft/crates/aft/src` for `bindings.config` reads).
- [ ] Hand AFT the paired `subc-protocol` + `subc-transport` versions before
      publishing.
- [ ] Confirm each module repo (quota, alfonso-routing, llm-runner, credentials)
      drops its construction site in the same upgrade.
- [ ] Daemon-and-modules-together deploy (the missing-field skew makes new-module
      + old-daemon a hard reject).

## Release-train note (2026-06, shipping now)

This removal ships in `subc-protocol 0.5.0` together with the other three pending
wire changes (`HELLO_ACK.storage`, the `route.open` config-field removal,
`launch_nonce`), paired with `subc-transport 0.2.2` (re-pin to `^0.5`). NOT in this
train: `subc-client-rs` cannot publish yet — its consumer depends on `subc-control`
(`publish = false`), and no crates.io consumer pulls it (AFT has its own serve
impl; the other module repos path-dep subconscious). Publishing `subc-client-rs`
would first require publishing `subc-control`, deferred to a follow-on. The commons
storage crates are a separate train.

## Not doing now

The field is inert (unused, ignored on the wire), so this is a clarity/thin-core
cleanup, not a bug fix. It waits for the next batched `subc-protocol` wire-change
release (which already carries `HELLO_ACK.storage`, the `route.open` config-field
removal, and `launch_nonce`), so it shares one AFT coordination + one republish.
