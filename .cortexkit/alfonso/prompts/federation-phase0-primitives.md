# Build: federation phase-0 subc-core primitives (P1 catalog.update + P2 prefix reservation)

Implement §2.6 of `docs/subc-federation-design.md` (v4.1) in `crates/subc-core`
(+ the wire shapes in `crates/subc-protocol` / `crates/subc-control` as
needed). The spec survived two adversarial council gates; §2.6 is normative.
These are GENERAL registry primitives — zero federation logic lands in
subc-core.

## P1 — `catalog.update` (module-direction channel-0 op)

An already-registered module replaces ONLY the `provides` tools/ops list of
its manifest in place.

Semantics (normative, from §2.6):
- New `ModuleControlRequest`-side op? NO — this is MODULE→DAEMON direction
  (like HELLO), not daemon→module. Add a new frame body op the module sends
  on channel 0 (follow how HELLO is dispatched in control.rs; a Request
  frame with a tagged body op `catalog.update` carrying the replacement
  `provides: Vec<ProviderRole>`). Reply: Response ack or typed Error.
- Registry entry's manifest.provides replaced; catalog generation bumped
  (`bump_generation` — catalog.list consumers see it).
- Existing route bindings and forwarding state UNTOUCHED. No GOODBYE of any
  kind is emitted by this op.
- FROZEN FIELDS: the op carries only the provides list, but validate anyway:
  reject with error code `catalog_update_frozen_field` if the replacement
  would change the ROLE KIND set in a way that alters `manifest_concurrency`
  (control.rs — concurrency derives from the ToolProvider role's concurrency
  field) — i.e. if the new provides' derived concurrency differs from the
  registered connection's stored concurrency, reject. control_ops are not in
  the manifest (separate HELLO field) so they cannot change via this op by
  construction — note that in a comment.
- Sender validation: the op is only accepted from the connection that OWNS
  the registration (connection_id match); a non-registered connection or a
  mismatched connection gets a typed error (`not_registered`).
- Role-gating interaction: a module registered supervision-only (empty
  provides) may NOT use catalog.update to become routable (that would bypass
  the HELLO-time forwarding registration) — reject any update that changes
  provides between empty and non-empty in either direction with
  `catalog_update_frozen_field`. (Routability is a HELLO-time property; the
  registered forwarding state was created — or not — at HELLO.)
- Capability advertisement: add `catalog.update` to the `subc_ops` list the
  daemon reports in HELLO_ACK (see subc_ops() in control.rs) so modules can
  detect support.

## P2 — namespace-prefix reservation

Extend reserved-module protection from exact ids to prefixes.

Semantics (normative, from §2.6):
- Daemon config (`daemon_config.rs`): a module entry gains an optional
  `reserved_prefixes: ["fed:"]` list (only meaningful with the module also
  being spawn-supervised). Validation at config load: every prefix MUST end
  with `:`; overlapping prefixes across different owner modules are rejected
  (a prefix that is a prefix of another owner's prefix = config error);
  a reserved prefix may not collide with any configured exact module id.
- SupervisorHandle: alongside `reserved_nonces` (exact) add prefix →
  owner-module mapping. HELLO authorization: if the claimed module_id
  matches a reserved prefix (delimiter-aware starts_with) AND is not an
  exact-reservation match (exact takes precedence), the HELLO must present
  the OWNER module's current spawn nonce; otherwise reject with the existing
  reserved-module error shape (`reserved_module` code, message naming the
  prefix).
- Boundary-case test matrix (required, from the council): `fed:` matches
  `fed:peerA:tool` and `fed:x`; does NOT match `fedx:tool`, `fed`, or `FED:x`;
  exact reservation of `fed:special` wins over prefix `fed:` (different
  nonce); overlapping config (`fed:` owned by A, `fed:sub:` owned by B)
  rejected at load; prefix + non-supervised owner rejected at load.
- Threat-model comment in code (from §2.6): this is NOT a same-user barrier
  (same-user can read the key file and env); it protects against accidental
  collisions and lower-trust processes, same as exact-id reservation.

## Tests

- P1: integration test (existing TestServer patterns in
  crates/subc-core/tests/): register stub with tools [a,b] → catalog.list
  shows both → open route, start a call → catalog.update to [a,c] →
  catalog.list reflects it, generation bumped, in-flight call completes,
  route still live → catalog.update attempting concurrency-changing or
  empty↔non-empty provides → `catalog_update_frozen_field` → update from a
  different connection → `not_registered`.
- P2: the boundary matrix above, plus: squatting attempt (HELLO claiming
  `fed:peerX:aft` without owner nonce → rejected; with owner nonce →
  accepted); exact-reservation precedence.
- Golden JSON for any new wire shapes (follow crates/subc-control/tests
  golden patterns; new body shapes need vectors).
- Level-triggered sync, no sleeps-as-sync, 10s setup helper (repo norm).

## Gates

Workspace cargo test green (env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE);
clippy -D warnings native + x86_64-pc-windows-gnu; fmt; check_comments.
Comments carry reasons; the §2.6 spec is the reference — cite it where the
semantics are non-obvious.
