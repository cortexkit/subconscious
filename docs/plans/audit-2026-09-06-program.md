# Start-over audit, 2026-09-06: the five we are doing

Six independent model audits of this repo answered "if you started over, what
would you do differently". Their claims were checked against source before this
list was written; the ones that survived and were approved are below, in the
order they will be worked. Each item is a standing task for idle time on this
seat, not a campaign with a deadline.

Verified findings the list rests on (numbers read from the tree, not the audits):

- `subc-core` is one crate holding the daemon (`lib.rs`), the operator CLI
  (`bin/ck.rs`, 7,031 lines), and the setup/upgrade engine (`setup/`, 23 files,
  `#[path]`-included into `ck.rs` only). The crate therefore carries `rusqlite`
  (bundled), `reqwest`, and `ed25519-dalek` for code the daemon never links.
  Twelve sibling modules dev-depend on `subc-core` for `bootstrap::run_with_config`
  (an in-process daemon for their tests), `Frame`, `ServerError`.
- `ModuleManifest` requires `trust_tier`, `consumes`, and `bindings` on the wire
  and in the builder; the daemon reads none of them on any production path.
- The four wire crates are `publish = true` and on crates.io, but the registry is
  behind the tree by several minor versions (protocol 0.10.0 published, 0.18.0
  here). Twenty-seven sibling checkouts path-depend on them instead.
- Timing budgets are synchronized by comment: the SDK route-open retry deadline
  (30s) "matches" the daemon drain ceiling (30s); liveness probe windows and the
  bind-relay budget (12s) are copies. Nothing fails when one side moves.
- Retry/close decision tables are hand-mirrored in TS, Rust, and Swift with
  comments saying "kept identical to".
- The connection reader awaits routing inline (`server.rs`) and a request awaits
  route credit on that same task (`router.rs`); a saturated route blocks CANCEL
  and unrelated routes on the connection. The loom-checked single-owner design in
  `dispatch_spike/` is `#[cfg(test)]`, waiting on production evidence.

## The five

| # | Item | Shape | Fleet cost | Status |
|---|------|-------|-----------|--------|
| 1 | **Contract tables as fixtures, not comments.** Budgets (drain, retry deadline, bind relay, probe windows, auth deadline, arbitration grace) and decision tables (retryable route-open codes, close-reason dispositions) live in `crates/subc-protocol/tests/golden/`; daemon and all three SDKs assert against them the way `MAX_FRAME_BODY_LEN` is asserted today. Codegen only if drift recurs after the fixtures exist. | fixture + parity tests | none (tests only) | in flight |
| 2 | **Publish on every bump.** The wire-crate release chain publishes to crates.io as part of the version bump, so a consumer can pin the registry instead of the sibling path. Path deps stay supported; the notice invites, never forces. | release script + CI workflow | one notice | queued |
| 3 | **Manifest diet.** `trust_tier`, `consumes`, `bindings` become `Option` + `serde(default)`; the builder stops requiring them; the daemon keeps decoding old manifests that carry them. | protocol bump | one lock wave (builder signature) | in flight |
| 4 | **Crate split.** `subc-core` keeps the daemon library (which is what the twelve dev-dep consumers use); `ck`, `ck-under-test`, `setup/`, `fleet_lint`, `subc-probe`, `fake-aft-stub` move to a new `ck` crate. No behaviour change; the daemon's dependency graph loses the installer's. | workspace move | one `subc-core` bump for dev-dep consumers | queued |
| 5 | **Module lifecycle authority.** One record per module with incarnation-fenced transitions (registered, admission closed, drained, exited, replaced); `handle_route_open`'s six locked reads become one; public status is a projection. Not the per-frame actor. | design room → athena → spec campaign | daemon-internal | needs a room |

Batched into waves so each fleet cost is paid once:

- **Wave A** = items 1 + 3 → one `subc-protocol` bump, one lock wave.
- **Wave B** = item 4 → one `subc-core` bump.
- **Wave C** = item 2 → publish chain, then the invitation notice.
- **Wave D** = item 5 → room first.

## Audit claims that were checked and declined

- Direct peer IPC instead of the splice router: trades the channel table for N×M
  socket management and moves `Principal` attestation out of the daemon; the
  epoch/handle machinery it blames is what makes teardown honest.
- gRPC / Cap'n Proto instead of the 21-byte envelope over opaque JSON: the daemon
  never parsing bodies is the design; a schema-owning daemon is a different product.
- Unix sockets + peer credentials instead of loopback TCP + key: correct
  analysis; ruled "stay on TCP" on 2026-08-10, and the remote story is Noise.
- Machine-owned config + human override file: right at t=0; the comment-preserving
  editor already exists and is small.
