//! Forwarding-table contention baseline (current Mutex design).
//!
//! Run: `cargo bench -p subc-core --bench forwarding_contention --features bench-harness`

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use subc_core::bench_harness::{bench_client_forward_op, build_bench_forwarding_setup, BenchClientRoute};
use tokio::runtime::Builder;

const WARMUP_ITERS: u64 = 1_000;
const MEASURE_ITERS: u64 = 10_000;
const ARM1_CLIENTS: &[usize] = &[1, 2, 4, 8, 16, 32];
const ARM1_ROUTES: &[usize] = &[1, 8, 64];

#[derive(Clone, Copy)]
struct LatencySnapshot {
    p50_us: f64,
    p99_us: f64,
    p999_us: f64,
    throughput_ops: f64,
}

fn main() {
    let rt = Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(4))
        .enable_all()
        .build()
        .expect("tokio runtime");

    println!("=== subc forwarding contention benchmark (BASELINE / current design) ===");
    println!(
        "host: {} | logical CPUs: {}",
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    println!("arm1: in-process client forward (Router pre-lookup + ForwardBackend)");
    println!("warmup={WARMUP_ITERS} measured={MEASURE_ITERS} per cell\n");

    println!(
        "{:<6} {:<8} {:>10} {:>10} {:>10} {:>14}",
        "clients", "routes", "p50_us", "p99_us", "p999_us", "ops/s"
    );
    println!("{}", "-".repeat(68));

    for &num_clients in ARM1_CLIENTS {
        for &routes_per_client in ARM1_ROUTES {
            let snap = rt.block_on(run_arm1_cell(num_clients, routes_per_client));
            println!(
                "{:<6} {:<8} {:>10.2} {:>10.2} {:>10.2} {:>14.0}",
                num_clients,
                routes_per_client,
                snap.p50_us,
                snap.p99_us,
                snap.p999_us,
                snap.throughput_ops
            );
        }
    }

    println!("\narm2: loopback end-to-end — run integration test:");
    println!("  cargo test -p subc-core forwarding_bench_e2e_arm2 -- --ignored --nocapture");
}

async fn run_arm1_cell(num_clients: usize, routes_per_client: usize) -> LatencySnapshot {
    let setup = build_bench_forwarding_setup(num_clients, routes_per_client).await;
    let forwarding = Arc::clone(&setup.forwarding);
    let forward_backend = setup.forward_backend.clone();
    let routes: Arc<Vec<BenchClientRoute>> = Arc::new(setup.client_routes);

    let mut handles = Vec::with_capacity(num_clients);
    for client_index in 0..num_clients {
        let forwarding = Arc::clone(&forwarding);
        let forward_backend = forward_backend.clone();
        let routes = Arc::clone(&routes);
        let base = (client_index * routes_per_client) as u64;
        handles.push(tokio::spawn(async move {
            let mut latencies_ns = Vec::with_capacity(MEASURE_ITERS as usize);
            let mut corr = base.wrapping_mul(1_000_000);
            for _ in 0..WARMUP_ITERS {
                let route = &routes[client_index * routes_per_client];
                let _ = bench_client_forward_op(
                    &forwarding,
                    &forward_backend,
                    route,
                    corr,
                )
                .await;
                corr = corr.wrapping_add(1);
            }
            let wall_start = Instant::now();
            for _ in 0..MEASURE_ITERS {
                let route_idx = client_index * routes_per_client
                    + (corr as usize % routes_per_client);
                let route = &routes[route_idx];
                let t0 = Instant::now();
                bench_client_forward_op(&forwarding, &forward_backend, route, corr)
                    .await
                    .expect("forward op");
                latencies_ns.push(t0.elapsed().as_nanos() as u64);
                corr = corr.wrapping_add(1);
            }
            let wall = wall_start.elapsed();
            (latencies_ns, wall)
        }));
    }

    let mut all_latencies = Vec::new();
    let mut total_wall = Duration::ZERO;
    for handle in handles {
        let (mut latencies, wall) = handle.await.expect("worker");
        all_latencies.append(&mut latencies);
        if wall > total_wall {
            total_wall = wall;
        }
    }

    all_latencies.sort_unstable();
    let n = all_latencies.len();
    let idx = |pct: f64| -> usize {
        ((pct * (n as f64 - 1.0)).round() as usize).min(n.saturating_sub(1))
    };
    let to_us = |v: u64| v as f64 / 1_000.0;
    let total_ops = (num_clients as u64) * MEASURE_ITERS;
    let throughput = total_ops as f64 / total_wall.as_secs_f64();

    LatencySnapshot {
        p50_us: to_us(all_latencies[idx(0.50)]),
        p99_us: to_us(all_latencies[idx(0.99)]),
        p999_us: to_us(all_latencies[idx(0.999)]),
        throughput_ops: throughput,
    }
}
