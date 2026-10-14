# Forwarding-table contention benchmark (BASELINE)

Measures cost of subc's **current** `Mutex<ForwardingInner>` on the data-plane hot path before any RwLock / lookup-fusion refactor. Label: **BASELINE / current design**.

## Commands

**Arm 1 — in-process micro-bench** (release, multi-thread Tokio):

```bash
cargo bench -p subc-core --bench forwarding_contention --features bench-harness
```

**Arm 2 — loopback TCP + `fake-aft-stub`** (debug test binary; subprocess + I/O):

```bash
cargo test -p subc-core --test forwarding forwarding_bench_e2e_arm2 -- --ignored --nocapture
```

(`eprintln!` table is on stderr; redirect with `2> arm2.txt` if needed.)

## Methodology

### Arm 1

- Builds a real `ForwardingTable` via production APIs: `register_module_connection`, `begin_route_bind_relay_for`, `complete_pending_relay`, `commit_route` (`crates/subc-core/src/bench_harness.rs`, feature `bench-harness`).
- One tool-provider module (`StatelessParallel` concurrency), **N** synthetic client connections, **M** committed routes each.
- A background task echoes module REQUESTs to client RESPONSEs and releases flow credits (same shape as stub echo), so `ForwardBackend::handle` does not stall on the 1024-credit window.
- **N** concurrent Tokio tasks on a multi-thread runtime (`worker_threads` >= logical CPUs). Each iteration: `ForwardingTable::client_route` (router pre-check), then `ForwardBackend::handle` (second `client_route`, optional `endpoint_is_draining`, credit acquire, channel rewrite, `FrameSink::send`).
- Per-op latency: `Instant` around that pair; aggregate p50/p99/p999 over all client samples; throughput = total ops / max worker wall time.
- Sweep: clients **N in {1,2,4,8,16,32}**, routes **M in {1,8,64}**. Warmup **1000**, measured **10000** ops per client per cell.

### Arm 2

- Reuses `tests/forwarding.rs` rig: `TestServer`, `connect_authed_client`, `spawn_stub` / `fake-aft-stub`, `attach_on_stream`, fixed small JSON-RPC payload.
- Concurrent clients on one daemon; per-client sequential tool-call round-trips (write REQUEST, read RESPONSE).
- Sweep: **(N,M) in {(1,1),(8,1),(32,1),(8,8),(32,8)}**. Warmup **100**, measure **1000** calls per client.

## Code map (no production routing behavior changes)

- `crates/subc-core/src/bench_harness.rs` — table setup + bench op (`bench-harness` feature).
- `crates/subc-core/benches/forwarding_contention.rs` — arm1 driver.
- `crates/subc-core/tests/forwarding.rs` — `forwarding_bench_e2e_arm2` (ignored).

Post-refactor AFTER results are recorded above for Arm 1.
