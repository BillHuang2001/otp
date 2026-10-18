# m16 host/load record (2026-10-18)

The benchmark host is **shared and heavily loaded** for this session: `/proc/loadavg` 1-min ranged
**3–76** across the whole session (m15 ran at 2–5). Absolute wall times and throughputs are therefore
load-contaminated on both sides; read the **ratios** and the deterministic `case_*_ops/calls/elements`
counters, never a single absolute figure as a constant.

Every A/B run is pinned with `taskset -c 3` (a P-core). The two single-side passes that have no A/B
counterpart (the external targets, `plb`, `emqx`) are left **unpinned** so their figures stay comparable
with m15's, which was also unpinned.

| run | artifact(s) | pin | `/proc/loadavg` (1-min) before → after |
|---|---|---|---|
| primary cross-runtime (`--full`, runtime-major) | `m16-bench-xrun.json` | `taskset -c 3` | 15.49 → 9.41 (5-min 44.13 → 12.86) |
| matched cross-runtime rounds 1–3 (rust, sys alternating) | `m16-xab-{rust,sys}-r{1,2,3}.json` | `taskset -c 3` | r1 7.62→8.25 (rust), 8.25→9.13 (sys); r2 9.13→8.01 (sys), 8.01→7.99 (rust); r3 7.99→4.98 (rust), 4.98→8.63 (sys) |
| binary A/B, rounds 1–3 (new vs m15-era runtime) | `m16-ab-{new,old}-r{1,2,3}.json` | `taskset -c 3` | r1 55.85→76.30 / 76.67→55.42; r2 76.67→55.42 / 55.42→20.89; r3 20.89→10.98 / 10.98→7.84 |
| `binary_append` direct ladder (old vs new) | this file, §"append ladder" | `taskset -c 3` | 15.68 → 54.88 |
| tier control (`BEAM_RT_TIER=interpreter`, rust-only) | `m16-bench-xrun-interp.json` | `taskset -c 3` | 7.01 → 3.04 |
| external targets (`--full`, sys-only) | `m16-bench-external.json` | none | 3.04 → 8.94 |
| `plb` / `emqx` (`--quick`, sys-only) | `m16-bench-plb.json`, `m16-bench-emqx.json` | none | 4.66 → 4.66 |
| pinning control (`baseline`, pinned vs unpinned) | `m16-pin-control-{pinned,unpinned}.json` | both | 28.06 → 20.72 |

Raw logs: `m16-xrun-load.log`, `m16-xab-load.log`, `m16-ab-load.log`, `m16-ext-load.log`.

## The pinning control

`baseline` was run back to back pinned and unpinned at load 28.06 (`--quick`, both runtimes):

| side | startup p50 (pinned) | startup p50 (unpinned) | p99 (pinned / unpinned) |
|---|---|---|---|
| sys | 105.8 ms | 109.1 ms | 107.1 / 117.6 ms |
| rust | 395.8 ms | 402.6 ms | 396.4 / 407.1 ms |

Pinning is **neutral for the mean and strictly tighter for the tail** on both sides (rust p99 spread
7 ms vs 10 ms; sys 3 ms vs 10 ms), so `taskset -c 3` is adopted for every A/B in this session and it is
not the cause of any cross-session difference.

## The append ladder (old vs new runtime, `taskset -c 3`)

One `bench_binary_append:run/1` process per sample, each call appending 16 384 four-byte segments into
one writable payload (16 KiB in → 64 KiB out):

| N calls | m15-era runtime (`a34ca61332`) | current runtime | note |
|---|---|---|---|
| 10 | 62 583 ms (≈6.3 s/call) | 434 ms (≈4.4 ms/call after subtracting the ≈390 ms boot) | |
| 30 | 217 001 ms (≈7.2 s/call) | 579 ms (≈6.3 ms/call) | |
| 100 | >300 s, killed (its own run deadline fired) | 884 ms (≈4.9 ms/call) | |

So the runtime-level append path is **≈1 400× cheaper** than the m15-era binary, and the current
runtime's own per-call cost drifts upward with `N` (4.4 → 4.9 ms over N=10 → 100 → the 3.24 ms marginal at
N=2 000 measured through the paired estimator) — the documented `N`-sensitivity of the loop workloads.
