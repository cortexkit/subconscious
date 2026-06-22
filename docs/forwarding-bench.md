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

## BASELINE results

Captured on **aarch64**, **18 logical CPUs**, **2026-06-22** (release bench for arm1; debug test for arm2).

### Arm 1 — client forward (us, ops/s)

| clients | routes | p50_us | p99_us | p999_us | ops/s   |
|--------:|-------:|-------:|-------:|--------:|--------:|
| 1       | 1      | 0.08   | 6.46   | 8.83    | 3937913 |
| 1       | 8      | 0.08   | 7.42   | 11.29   | 4079619 |
| 1       | 64     | 0.12   | 11.04  | 18.17   | 1759260 |
| 2       | 1      | 0.17   | 17.54  | 33.96   | 1484014 |
| 2       | 8      | 0.21   | 15.29  | 27.12   | 1276199 |
| 2       | 64     | 0.21   | 16.12  | 29.38   | 1329865 |
| 4       | 1      | 0.17   | 38.25  | 73.96   | 1342721 |
| 4       | 8      | 0.21   | 28.83  | 47.42   | 1337813 |
| 4       | 64     | 0.25   | 30.46  | 46.88   | 1286898 |
| 8       | 1      | 0.25   | 82.67  | 155.29  | 1126309 |
| 8       | 8      | 0.33   | 64.46  | 120.46  | 1217878 |
| 8       | 64     | 0.42   | 58.38  | 90.08   | 1101227 |
| 16      | 1      | 0.29   | 243.79 | 735.79  | 1087471 |
| 16      | 8      | 7.71   | 86.54  | 270.50  | 837450  |
| 16      | 64     | 7.17   | 97.67  | 247.04  | 857929  |
| 32      | 1      | 0.33   | 454.08 | 3206.04 | 1088180 |
| 32      | 8      | 51.33  | 118.75 | 292.88  | 636259  |
| 32      | 64     | 52.83  | 125.46 | 331.42  | 605272  |

### Arm 1 — client forward (us, ops/s), AFTER (fusion + RouteBinding + RwLock)

Captured on **aarch64**, **18 logical CPUs**, **2026-06-22** (same machine/command as BASELINE).

| clients | routes | p50_us | p99_us | p999_us | ops/s   |
|--------:|-------:|-------:|-------:|--------:|--------:|
| 1       | 1      | 0.29   | 1.29   | 8.83    | 3188479 |
| 1       | 8      | 0.17   | 1.67   | 10.04   | 3887647 |
| 1       | 64     | 0.17   | 2.67   | 23.75   | 3361580 |
| 2       | 1      | 0.42   | 10.67  | 23.08   | 2879148 |
| 2       | 8      | 0.38   | 2.29   | 7.62    | 4559531 |
| 2       | 64     | 0.33   | 4.00   | 20.83   | 4468899 |
| 4       | 1      | 0.38   | 23.79  | 49.46   | 2647436 |
| 4       | 8      | 0.50   | 6.75   | 19.17   | 5083938 |
| 4       | 64     | 0.50   | 4.21   | 20.88   | 5710988 |
| 8       | 1      | 0.42   | 43.75  | 94.71   | 2230209 |
| 8       | 8      | 1.04   | 15.71  | 37.17   | 3974522 |
| 8       | 64     | 1.04   | 34.04  | 56.00   | 2796636 |
| 16      | 1      | 0.46   | 61.04  | 386.54  | 2092891 |
| 16      | 8      | 1.62   | 54.50  | 81.21   | 1260014 |
| 16      | 64     | 2.04   | 59.50  | 86.38   | 1194383 |
| 32      | 1      | 0.42   | 103.54 | 1556.29 | 2245813 |
| 32      | 8      | 41.00  | 91.17  | 123.04  | 809348  |
| 32      | 64     | 43.29  | 93.33  | 125.33  | 766737  |

### Arm 1 delta vs BASELINE

After-vs-baseline deltas; negative latency is faster, positive throughput is higher. The refactor improves p99 in every cell and p999 in 16/18 cells; p50 regresses in low-contention cells where `RwLock`/`Arc` overhead dominates, but improves for the high-contention 16×{8,64} and 32×{8,64} cells.

| clients | routes | p50_delta | p99_delta | p999_delta | ops_delta |
|--------:|-------:|----------:|----------:|-----------:|----------:|
| 1       | 1      | +262%     | -80%      | +0%        | -19%      |
| 1       | 8      | +112%     | -77%      | -11%       | -5%       |
| 1       | 64     | +42%      | -76%      | +31%       | +91%      |
| 2       | 1      | +147%     | -39%      | -32%       | +94%      |
| 2       | 8      | +81%      | -85%      | -72%       | +257%     |
| 2       | 64     | +57%      | -75%      | -29%       | +236%     |
| 4       | 1      | +124%     | -38%      | -33%       | +97%      |
| 4       | 8      | +138%     | -77%      | -60%       | +280%     |
| 4       | 64     | +100%     | -86%      | -55%       | +344%     |
| 8       | 1      | +68%      | -47%      | -39%       | +98%      |
| 8       | 8      | +215%     | -76%      | -69%       | +226%     |
| 8       | 64     | +148%     | -42%      | -38%       | +154%     |
| 16      | 1      | +59%      | -75%      | -47%       | +92%      |
| 16      | 8      | -79%      | -37%      | -70%       | +50%      |
| 16      | 64     | -72%      | -39%      | -65%       | +39%      |
| 32      | 1      | +27%      | -77%      | -51%       | +106%     |
| 32      | 8      | -20%      | -23%      | -58%       | +27%      |
| 32      | 64     | -18%      | -26%      | -62%       | +27%      |

In the baseline, tail latency grows sharply at **16-32** concurrent lock contenders (p99/p999), while median stays sub-microsecond to low tens of us.

### Arm 2 — loopback e2e (ms, calls/s)

| clients | routes | p50_ms | p99_ms | calls/s |
|--------:|-------:|-------:|-------:|--------:|
| 1       | 1      | 0.083  | 0.174  | 9299    |
| 8       | 1      | 0.173  | 0.279  | 40588   |
| 32      | 1      | 0.543  | 0.719  | 53037   |
| 8       | 8      | 0.176  | 0.303  | 40017   |
| 32      | 8      | 0.888  | 1.313  | 32434   |

End-to-end latency is dominated by socket I/O and stub process at these scales; arm1 isolates forwarding-table mutex contention.

## Code map (no production routing behavior changes)

- `crates/subc-core/src/bench_harness.rs` — table setup + bench op (`bench-harness` feature).
- `crates/subc-core/benches/forwarding_contention.rs` — arm1 driver.
- `crates/subc-core/tests/forwarding.rs` — `forwarding_bench_e2e_arm2` (ignored).

Post-refactor AFTER results are recorded above for Arm 1.
