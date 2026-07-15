# Wire v1-final — Wave 2: subc-mcp (gateway module)

You are migrating subc-mcp to a frozen, Oracle-gated wire revision. Authoritative spec: `docs/specs/subc-wire-v1-final.md` (v7) — READ IT FULLY, especially §3.2.1 (RouteHandle), §3.3 layer 2 (endpoint validation), §7's subc-mcp row (your migration is enumerated there precisely), §10. Wave 1 landed the wire shapes on master (21-byte header + epoch, RouteBind.epoch, RouteOpen.route_epoch...). The daemon logic and subc-client-rs land in PARALLEL waves — subc-mcp hand-rolls its own frames (SubcClient in main.rs), so your work is self-contained against subc-protocol/subc-transport; your tests are unit/harness level, not live-daemon. Grep `// WIRE-WAVE2:` for wave-1 stopgaps in this crate and replace them.

## Scope: crates/subc-mcp ONLY.

The spec §7 enumeration is your checklist — ALL bare-channel state migrates to RouteHandle/(channel, epoch, corr):

1. **Route state**: `SessionInner.routes` / `ToolBinding` — store `(channel, epoch)` handles (with the module's consumer-connection generation as the token; a reconnect mints fresh handles and no pre-reconnect handle may emit on the new connection).
2. **Pending keys**: `PendingKey = (u16, u64)` → `(u16, u32, u64)` (channel, epoch, corr) everywhere it keys in-flight state.
3. **ReverseRelay**: `routes` and `pending` maps move to handle keys. REVERSE REPLIES RETAIN THE INGRESS HANDLE (spec: never look up "the current epoch" at reply time) — the host's elicitation reply is stamped with the (channel, epoch) the reverse Request ARRIVED on; if that binding died meanwhile, the daemon drops the stale reply, which is correct.
4. **Reader-loop validation** (§3.3 layer 2): `subc_reader_loop` maintains channel→epoch for live routes and validates every nonzero-channel ingress frame BEFORE any dispatch (type-first reverse dispatch included) — mismatch/unknown → silent drop + counter.
5. **route.open plumb**: consume route_epoch; install before use; the private binding-table (search-mode) entries carry handles.
6. **Egress stamping**: every route frame the module emits carries the route's epoch; channel-0 frames carry epoch 0. All Frame::build call sites updated.
7. **Corr hygiene**: `SubcClient::next_corr` currently wraps — make it monotonic no-reuse per connection (close+reconnect on exhaustion, unreachable in practice).
8. **Bind sequence invariant** (hand-rolled form, §3.2): no route-scoped egress frame for a binding may precede that binding's RouteBind ack in the writer queue. Verify the current bind path satisfies it (it should — acks are sent from the loop before any traffic); add the assertion to the wire tests.
9. **Health/supervision connection**: unchanged semantics (channel-0 only, epoch 0); update frame construction.

## Tests
Existing test suite migrated to 21-byte frames + epochs. New per §10: an E1 host reply arriving after E2 slot reuse is dropped by the retained-ingress-handle rule (never delivered to the E2 session); stale-epoch ingress drop in the reader loop (no dispatch, no callback); corr monotonic no-reuse; bind-invariant assertion (no route egress precedes the ack in the writer queue).

## Verification bar
`cargo test --workspace` green; `cargo clippy --workspace --all-targets` + `cargo clippy -p subc-mcp --target x86_64-pc-windows-gnu --all-targets` clean; `cargo fmt --all`; zero `// WIRE-WAVE2:` stopgaps remain in this crate; zero bare-channel keys remain (grep PendingKey and the binding tables).

Commit clearly; report per-item status + test totals.
