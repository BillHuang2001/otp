//! Benchmark driver for the external target projects plus a system-ERTS baseline.
//!
//! Drives the pure-Erlang/Elixir workloads named in Part I §C (Cowboy, Khepri, the
//! Elixir compiler, Benchmarks-Game kernels, EMQX), grouped by the benchmark
//! categories of Part I §C — compute/JIT-bound, messaging/copy-bound, GC-bound and
//! I/O-bound — and measures startup latency, p99 tail latency, throughput and
//! memory against the system Erlang baseline. Placeholder entry point until the
//! harness child architect lands the runner.

fn main() {
    println!(
        "bench {} — Rust ERTS benchmark driver (placeholder)",
        env!("CARGO_PKG_VERSION"),
    );
}
