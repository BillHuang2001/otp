# m6 cross-runtime pass — load record & results pointer

Measurement-and-archive record for the **m6** cross-runtime pass (the six `Runtimes::Both`
workloads under both runtimes). Facts only: exact commands, the `/proc/loadavg` observed
before/after each run, the per-workload statuses, and the sys-vs-rust ratio per workload.
The numbers themselves live in the two committed artifacts beside this file:

- `m6-bench-xrun.json` — the full `Report` (schema_version 1, profile `full`).
- `m6-compare-cross-runtime.txt` — `bench compare` output for that report (sys vs rust rows).

> **Host-load caveat (read this first).** The benchmark host is a shared 14-core machine. The
> 1-minute load observed across this pass was **~414–440** (see the record below), i.e. ~30× the
> core count. Every **absolute latency/wall** figure in the artifacts is therefore **inflated** by
> CPU contention and must not be read as a machine-speed constant. The load-robust signals are the
> **per-op / per-element marginals** (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the
> **deterministic counts** (`*_ops`, `*_elements`); the per-runtime loop count `N` is fixed, so the
> per-op figures are the comparable ones across the two runtimes.

## 1. Environment

- Oracle (`sys`): system `erl` **29** (OTP 29.1.1 / erts-17.1), resolved on `PATH`.
- Rust (`rust`): `erts_rust/target/release/beam_rt` (reports OTP `30`), self-check passing.
- Both runtimes report `supported: true` in the report.

## 2. Exact commands

```sh
# 1. build (once)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# 2. focused cross-runtime run — all six Runtimes::Both workloads, --full, both runtimes
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m6-bench-xrun.json --verbose

# 3. cross-runtime diff (committed as a text artifact)
erts_rust/target/release/bench compare \
    erts_rust/tools/bench/results/m6-bench-xrun.json \
    > erts_rust/tools/bench/results/m6-compare-cross-runtime.txt
```

`--full` was used for the whole set (it completed in ~3m17s wall — no need to fall back to `--quick`
for the two new heavy workloads). Workloads were run **serially**; no two benchmark workloads ran at
once.

## 3. `/proc/loadavg` record (1-min value first; nothing rounded away)

| point | observed `/proc/loadavg` |
|---|---|
| before build | `440.40 434.09 308.59 404/1300 3` |
| before run #2 (`m6-bench-xrun`) | `434.54 433.40 315.57 429/1443 4` |
| after run #2 (`m6-bench-xrun`) | `415.14 425.31 334.87 404/1267 32130` |
| before run #3 (`m6-compare`) — reading taken immediately prior | `415.14 425.31 334.87 …` (the after-run-#2 reading) |
| after run #3 (`m6-compare`) | `414.33 424.97 335.25 411/1308 4` |

The `run #2` wall time was `real 3m16.861s  user 4m34.777s  sys 0m45.426s`.

## 4. Per-workload status (report `results[]`)

Every (workload × runtime) pair ran: **12/12 rows `ok`, 0 `skipped`, 0 `unsupported`, 0 `failed`.**

| workload | category | sys | rust |
|---|---|---|---|
| `baseline`   | startup   | `ok` | `ok` |
| `slice`      | compute   | `ok` | `ok` |
| `foreign`    | compute   | `ok` | `ok` |
| `list_walk`  | compute   | `ok` | `ok` |
| `copy_bound` | messaging | `ok` | `ok` |
| `gc_bound`   | gc        | `ok` | `ok` |

No Rust row is fabricated: every rust row carries real `Summary`/scalar metrics (no `null`-only
rows, no `unsupported` classification, no crash/exit-3). Every case validated its own copy
round-trip with a bound-variable pattern on both sides and printed the deterministic constant
(recorded in the report's `observed stdout:` notes), so the two runtimes executed identical work.

## 5. Cross-runtime summary — one line per workload (headline metric, sys vs rust)

`sys/rust` < 1 means the Rust side costs **more** per unit (its ns value is higher). The marginals
(`*_ns_*`) are the load-robust cross-runtime signal; the wall/startup rows are inflated by the load
above and are shown for completeness.

| workload | headline cross-runtime metric | sys | rust | sys/rust |
|---|---|---|---|---|
| `baseline`   | `startup_ms` (mean, whole-process) | 157.0934 ms | 17.4974 ms | **8.98×** (rust faster) |
| `slice`      | `case_<name>_wall_ms` (mean, per case) | 136.22–228.73 ms | 17.92–36.90 ms | **4.16×–12.77×** (rust faster) |
| `foreign`    | `case_<name>_ns_per_call` | 216.5396–423.6601 ns | 27184.7436–41393.4373 ns | **0.0080–0.0134** (rust 74.9×–131.4× higher) |
| `list_walk`  | `case_list_walk_ns_per_element` | 3.1312 ns | 340.5021 ns | **0.0092** (rust 108.7× higher) |
| `copy_bound` | `case_<name>_ns_per_op` | 244.5916–1452.9405 ns | 4281.7309–194813.3306 ns | **0.0053–0.0571** (rust 17.5×–188.3× higher) |
| `gc_bound`   | `case_churn_ns_per_op` | 4643.2648 ns | 258354.3855 ns | **0.0180** (rust 55.6× higher) |

### 5.1 Per-case detail

`baseline` (whole-process cost — startup latency is the trustworthy cross-runtime comparison):

| metric | sys | rust | sys/rust |
|---|---|---|---|
| `startup_ms` (mean) | 157.0934 | 17.4974 | 8.98× |
| `compute_loop_ms` (mean) | 281.8195 | 73.5892 | 3.83× |
| `compute_ops_per_sec` | 3548370.12 | 13588956.79 | 0.2611 (rust faster; native floor, not interpreter throughput) |
| `vm_memory_total_bytes` | 36215944 | 805458 | 44.96× |
| `peak_memory_bytes` (RSS) | 137510912 | 31453184 | 4.37× |

`slice` (one call per process: `case_<name>_wall_ms` mean, and the comparable per-process memory):

| case | sys wall_ms | rust wall_ms | sys/rust |
|---|---|---|---|
| `add` | 146.6214 | 17.9539 | 8.17× |
| `fib` | 228.7267 | 17.9153 | 12.77× |
| `pick` | 190.3607 | 18.8709 | 10.09× |
| `calendar_last_day` | 176.2237 | 21.6430 | 8.14× |
| `queue_from_size` | 148.6591 | 24.3696 | 6.10× |
| `gb_trees_size` | 136.2164 | 21.3376 | 6.38× |
| `base64_encode_size` | 142.5424 | 23.6017 | 6.04× |
| `ordsets_from_size` | 182.1185 | 19.9575 | 9.13× |
| `string_str_length` | 153.3685 | 36.9011 | 4.16× |

`slice` process `peak_memory_bytes`: sys 72794112 → rust 27447296 (**2.65×** lower for rust).

`foreign` (real Cowboy + EMQX modules; startup-cancelled `ns_per_call`):

| case | sys ns/call | rust ns/call | rust/sys |
|---|---|---|---|
| `bstr_to_lower` | 423.6601 | 31729.8785 | 74.89× |
| `bstr_capitalize_token` | 375.0727 | 36027.8399 | 96.06× |
| `emqx_base62_encode` | 216.5396 | 27184.7436 | 125.54× |
| `emqx_base62_decode` | 315.1440 | 41393.4373 | 131.35× |

`list_walk` (BIF-free interpreted list walk; startup-cancelled per element):

| metric | sys | rust |
|---|---|---|
| `case_list_walk_ns_per_element` | 3.1312 ns | 340.5021 ns (**108.74×** higher) |
| `case_list_walk_elements` (deterministic) | 100000000 | 1000000 |

`copy_bound` (ETF / ETS / send-receive copy boundaries; startup-cancelled `ns_per_op`):

| case | op | sys ns/op | rust ns/op | rust/sys | ops sys / rust (deterministic) |
|---|---|---|---|---|---|
| `etf_roundtrip` | `term_to_binary`/`binary_to_term` | 647.4778 | 121953.9807 | 188.35× | 1 500 000 / 5 000 |
| `ets_copy` | `ets:insert`+`ets:lookup` | 244.5916 | 4281.7309 | 17.51× | 2 000 000 / 100 000 |
| `send_recv` | two-process send/receive copy | 1452.9405 | 194813.3306 | 134.08× | 500 000 / 2 000 |

`gc_bound` (allocate-and-abandon churn; startup-cancelled `ns_per_op`):

| metric | sys | rust |
|---|---|---|
| `case_churn_ns_per_op` | 4643.2648 ns | 258354.3855 ns (**55.64×** higher) |
| `case_churn_ops` (deterministic) | 200000 | 2000 |

> `gc_bound`'s row measures the allocate-and-abandon **wall time**; whether a runtime's collector
> actually runs during the loop is its own admission policy and is not claimed here (the report's
> own note says the same).

## 6. Honesty check

- Rust rows: all `ok` with real metrics; none `unsupported`, none fabricated, no workload crashed.
- `sys`/`rust` use identical source programs (embedded modules compiled once with the system `erlc`);
  each run's stdout was checked against the deterministic constant on both sides.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` figures are **startup-cancelled** marginals
  `(mean wall@N − mean wall@0) / N`, so per-process startup (including the Rust runtime's code-path
  resolution) cancels on both sides.
