# CortexKit Module Storage Contract

Standardized persistence for every CortexKit module (any language), so module
developers consume one durable storage layer instead of reinventing it, and so the
whole system can move to cloud-backed persistence by flipping one config.

## The model

There is ONE central storage config. A module never decides its own backend or
location. The backend is chosen centrally; each module receives its own isolated
storage and opens it through a shared library:

- `backend = sqlite` → the module gets its own sqlite **file**.
- `backend = postgres` → the module gets its own **database** inside postgres + a DSN.
- `backend = cloud` (future) → the module gets a cloud handle; drop-in, no module change.

Two libraries provide the consumption side with identical semantics:

- **Rust**: `cortexkit-store` + `cortexkit-lease` (extracted from llm-runner's proven
  store-trait + sqlite-backend + postgres-sibling + conformance-harness design).
- **TypeScript**: `@cortexkit/store` (parity). Together with the existing
  `@cortexkit/subc-client` serve role, this is the TS module dev kit (wire + persistence).

## Locked decisions

1. **subc stays thin.** subc-core handles only strings (resolve central config → a
   per-module descriptor). It never opens a database, holds a connection, or depends on
   rusqlite / a postgres driver / a cloud SDK. The module (via the shared lib, which
   carries those deps) opens the descriptor and creates its own database/schema if
   absent on first connect.
2. **Backend is cloud-open from day one.** The descriptor backend and the lib's backend
   trait are an extensible set (`sqlite | postgres | cloud{…}`), never a closed pair a
   future cloud backend has to retrofit. Build sqlite now, postgres soon, cloud later,
   each a drop-in behind the same trait in both libs.
3. **Postgres isolation = database per module.** Each module gets its own database,
   `cortexkit_<sanitized_module_id>` (module ids sanitized: hyphen → underscore). Stronger
   isolation, and a module's database is an independent portable unit (aligns with
   cloud-portable persistence). The lib performs `CREATE DATABASE` if absent on first
   connect using the central admin DSN — subc never touches postgres.
4. **Per-module storage granularity.** One store/database per module. A project-scoped
   module partitions its own rows by an internal project key (it already receives
   `project_root` via the route bind), NOT one database per (module, project). The scope
   set stays extensible so a future per-(module, project) isolation option is additive.
5. **Single-writer lease is shared.** The proven epoch-CAS fence (OS advisory lock +
   persisted monotonic epoch, never-unlink) lives in `cortexkit-lease`, so no module
   re-implements it (and the Windows lock-classification class of bug cannot be
   re-introduced per module). The lease trait stays open for a distributed/cloud variant
   (epoch-CAS in the cloud DB) enabling cross-machine session portability.
6. **Module owns its domain only.** The module provides its store trait operations, its
   schema, its backend-specific DDL, and its queries. The lib provides the lease, backend
   mechanics, descriptor consumption, database/schema creation, and a trait-generic
   conformance harness. Each backend ships its own DDL; the trait + conformance harness
   guarantee parity (no cross-backend magic migrator).
7. **Locations.** Rust lib in `cortexkit/commons` (cross-product, published to crates.io,
   alongside `cortexkit-paths`; carries the heavier db deps). TS lib beside
   `@cortexkit/subc-client` in `subconscious/clients/`.
8. **Conventions.** sqlite path `~/.local/share/cortexkit/<module_id>/store.db`. A
   machine-global storage scope is added alongside the existing project scope.

## Build sequence

1. Lock this contract (Oracle review of the open questions below).
2. Build the Rust lib (`cortexkit-store` + `cortexkit-lease`) by extracting llm-runner's
   proven design; sqlite backend + lease + conformance harness; postgres-sibling-ready.
3. `alfonso-routing` is the FIRST consumer (its route-state store was held specifically to
   consume this rather than hand-roll one).
4. `llm-runner` MIGRATES onto the lib as the validating second consumer (proves the
   extraction is faithful against already-proven durability tests).
5. TS lib `@cortexkit/store` in parity (same contract, its own conformance suite).
6. Wire central policy: the config section + descriptor resolution + delivery.

## Resolved (Oracle review bg_23985991)

1. **Descriptor delivery → subc DELIVERS.** Add an optional, additive
   `storage: Option<StorageDescriptor>` to `ModuleHelloAckBody`. subc stays the single
   authority for central policy while still thin (resolves strings, opens nothing). This
   also reaches self-connecting TS providers (they read `HELLO_ACK` in
   `SubcProvider.connect`; supervisor env injection would not reach them). The descriptor
   the module receives is a RESOLVED, LEAST-PRIVILEGE runtime descriptor, never the raw
   central config, and it is a TEMPLATE (a future per-(module,project) backend can extend
   via an optional `route.bind.storage` override, since `project_root` only arrives at
   `route.bind`, after `HELLO_ACK`).
2. **Postgres credentials → scoped per-module runtime credential, not an admin DSN per
   module.** A provisioner path (admin API in the shared lib, or a one-time provision step)
   creates the per-module `database + role`; subc delivers only the resulting scoped
   runtime DSN (no `CREATEDB`, access only to the module's own db). Admin-DSN first-connect
   bootstrap is allowed ONLY behind an explicit "trusted local bootstrap" dev mode, never
   the production default. The exact provisioning mechanism is decided at the postgres-build
   phase; the descriptor is shaped to carry a scoped DSN now. Module-database naming uses
   `cortexkit_<slug>_<16hex>` (strict module-id validation + slug + hash), NOT a bare
   hyphen→underscore substitution (which collides: `a-b` vs `a_b`).
3. **Cloud-open trait → extract the PATTERN, not llm-runner's traits verbatim.**
   llm-runner's `MaterializedStore`/`Persister` are backend-agnostic but embed llm-runner
   DOMAIN types (`Message`, `Usage`, `WalRecord`, `BindIdentity`), so they are not a
   universal boundary. The shared lib provides: descriptor + open mechanics, namespace/key
   derivation, lease primitives, and conformance harness helpers. Each MODULE still defines
   its own domain store trait. For cloud-readiness the lease guard must NOT be a concrete
   `File`-owning struct (`lease.rs` returns a concrete `LeaseGuard` owning a `File`) — make
   it a trait object / enum, and make stale-epoch rejection part of the WRITE path (CAS on
   expected epoch), not just an advisory-lock side effect.
4. **Scope → explicit, not naming-derived.** Encode isolation as an explicit
   `isolation: "module"` plus a stable `storage_namespace`; do NOT bake the per-module
   assumption into path/db naming or lease keying. Split "data scope" (how the module
   partitions rows) from "database isolation" (how many physical dbs); the current
   `StorageScope::Project` conflates them and is too narrow. A future per-(module,project)
   db is then additive (the descriptor template + the optional `route.bind.storage`).
5. **AFT → future opt-in adopter.** Do not contort the descriptor around AFT's existing
   layout. The delivered descriptor is optional and managed-store-specific; its PRESENCE is
   the opt-in. AFT's manifest already declares `bindings.storage`, so that field must NOT be
   read as "must migrate to the shared store" — managed storage is signalled by the
   delivered descriptor, not the manifest binding.

## Correctness requirements (must hold in the shared lib)

- **Lease + WAL keys MUST include `module_id` + a backend/storage namespace.** Today
  `FileLeaseStore` and `FileWalStore` hash only `project_root/harness/session`; under a
  shared lease root two different modules would collide on the same session key. Namespace
  every key by module + backend.
- **Epoch fence must be enforced on the WRITE path for distributed/cloud.** Today the local
  OS advisory lock enforces single-writer and the epoch is only documented; the WAL append
  writes the supplied fence without a CAS. That is fine for the local single-process lock,
  but the cloud/distributed backend's write/append MUST CAS on the expected epoch. Bake the
  epoch-CAS into the write-path trait from the start.
- **Backend comes from the DESCRIPTOR, not the module manifest.** `StorageKind` is closed
  and SQLite-only; the actual backend is central-config policy delivered in the descriptor.
  The manifest binding declares only "wants managed storage + data-scope intent."
- **Cross-language parity needs shared GOLDEN VECTORS**, not just parallel Rust/TS suites:
  golden fixtures for descriptor shapes, module-id sanitization, lease-key derivation, and
  error codes (reuse the repo's golden-JSON drift pattern).
- **Known trust property (documented, acceptable under the same-host model):** the
  transport authenticates possession of the connection-file key, not the claimed
  `module_id` (HELLO trusts any non-empty id). So a process holding the key could claim
  another module's id and receive that module's descriptor. Acceptable while all key-holders
  are trusted same-host code; if credential-bearing descriptors ever need cross-module
  isolation, HELLO `module_id` must be authorized first.
