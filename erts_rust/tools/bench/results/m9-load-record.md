# m9 measurement pass — load record & results pointer

Measurement-and-archive record for the **m9** pass (the full cross-runtime run of the six
`Runtimes::Both` workloads under both runtimes, and the full sys-only external-target run).
Facts only: the exact commands, the `/proc/loadavg` observed before/after each run, and the
sys-vs-rust ratio per workload.
The numbers themselves live in the committed artifacts beside this file:

- `m9-bench-xrun.json` — the full cross-runtime `Report` (schema_version 1, profile `full`).
- `m9-compare-cross-runtime.txt` — `bench compare` output for that report (sys vs rust rows, 69 numeric rows).
- `m9-bench-external.json` — the sys-only external targets in one `--full` report
  (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`).
- `m9-bench-plb.json` — `failed(plb-build-failed)`.
- `m9-bench-emqx.json` — `skipped(emqx-requires-otp-28)`.

> **Host-load caveat (read this first).** The benchmark host is a shared, usually-oversubscribed
> 14-core machine. During this session the 1-minute `/proc/loadavg` ran **from 4.11 to 28.95**
> (see the record below) — up to ~2× the core count. Every **absolute latency/wall/throughput**
> figure in the artifacts is therefore **inflated** by CPU contention when the load was high and
> must not be read as a machine-speed constant. The load-robust signals are the **per-op /
> per-element / per-call marginals** (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the
> **deterministic counts** (`*_ops`, `*_elements`); the per-runtime loop count `N` is fixed, so the
> per-unit figures are the comparable ones across the two runtimes.
> **A runtime-major report can be biased by drifting load:** the runner emits all `sys` rows first,
> then all `rust` rows, so if the load drifts during a run the two sides are measured in different
> conditions. The full cross-runtime pass below fell from 11.01 to 6.02, i.e. the later (`rust`)
> side ran under a *lighter* load — read the ratios, not the absolute pair.

## 1. Environment

- Oracle (`sys`): system `erl` **29** (OTP 29.1.1 / erts-17.1) at
  `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`.
- Rust (`rust`): `erts_rust/target/release/beam_rt` (reports OTP `30`), self-check passing.
- Both runtimes report `supported: true` in the report.
- Host: NixOS 26.11 (Zokor), Linux 7.2.9, `nproc` = 14.
- `dotnet` **8.0.425** is on `PATH`; `clang` 21.1.8 is on `PATH`; `rebar3` is **not** on `PATH`
  (resolved from the Nix store by the driver's store scan); `/usr/bin/time` is absent.

## 2. Exact commands

```sh
# build (once)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# quick passes (not archived — only the full runs below are committed)
erts_rust/target/release/bench run --quick --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound
erts_rust/target/release/bench run --quick --runtime sys --workloads erlc_compiler,elixir_compiler
erts_rust/target/release/bench run --quick --runtime sys --workloads cowboy,khepri
erts_rust/target/release/bench run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m9-bench-plb.json  --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m9-bench-emqx.json --verbose

# FULL cross-runtime run — all six Runtimes::Both workloads, --full, both runtimes (wall 2m44s)
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m9-bench-xrun.json --verbose

# cross-runtime diff (committed as a text artifact)
erts_rust/target/release/bench compare \
    erts_rust/tools/bench/results/m9-bench-xrun.json \
    > erts_rust/tools/bench/results/m9-compare-cross-runtime.txt

# FULL sys-only external targets (one report; wall 3m00s)
erts_rust/target/release/bench run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m9-bench-external.json --verbose
```

Workloads were run **serially** (no two benchmark workloads at once).

## 3. `/proc/loadavg` record (1-min value first, in the order the passes were run)

| pass | before (1-min) | after (1-min) | artifact |
|---|---|---|---|
| quick cross-runtime | `11.98` | `12.80` | (not archived) |
| quick compilers (`erlc_compiler`, `elixir_compiler`) | `7.35` | `7.06` | (not archived) |
| quick `cowboy` + `khepri` | `6.66` | `28.95` | (not archived) |
| PLB quick attempt (fails in ~1.3 s) | `27.75` | `27.75` | `m9-bench-plb.json` |
| EMQX quick pass (skips immediately) | `4.11` | — | `m9-bench-emqx.json` |
| **FULL cross-runtime pass** (wall 2m44s) | `11.01` | `6.02` | `m9-bench-xrun.json` |
| **FULL external-targets pass** (wall 3m00s) | `4.68` | `4.43` | `m9-bench-external.json` |

The archived artifacts' own `generated_at_unix_ms` (UTC) bookend the session:
`m9-bench-plb.json` **13:25:38**, `m9-bench-xrun.json` **14:05:28**, `m9-bench-external.json`
**14:08:41**, `m9-bench-emqx.json` **14:09:45**. (The PLB attempt was the first archived run and the
EMQX observation the last; the operator's load table above keeps the order in which the passes were
driven — the EMQX quick pass was (re)confirmed at 14:09:45, after the full external run.)

## 4. Per-workload status (full cross-runtime pass, `m9-bench-xrun.json`)

Every (workload × runtime) pair ran: **12/12 rows `ok`, 0 `skipped`, 0 `unsupported`, 0 `failed`.**

| workload | category | sys | rust |
|---|---|---|---|
| `baseline`   | startup   | `ok` | `ok` |
| `slice`      | compute   | `ok` | `ok` |
| `foreign`    | compute   | `ok` | `ok` |
| `list_walk`  | compute   | `ok` | `ok` |
| `copy_bound` | messaging | `ok` | `ok` |
| `gc_bound`   | gc        | `ok` | `ok` |

No rust row is fabricated: every rust row carries real `Summary`/scalar metrics (no `null`-only
rows, no `unsupported` classification, no crash/exit-3). Every case validated its own copy
round-trip with a bound-variable pattern on both sides and printed the deterministic constant
(recorded in the report's `observed stdout:` notes), so the two runtimes executed identical work.

The full external pass (`m9-bench-external.json`) is **4 rows, all `ok`** (`erlc_compiler`,
`elixir_compiler`, `cowboy`, `khepri`). `m9-bench-plb.json` is **1 `failed`** row (no metric) and
`m9-bench-emqx.json` is **1 `skipped`** row (no metric).

## 5. Cross-runtime summary — one line per workload (headline metric, sys vs rust)

`sys/rust` < 1 means the Rust side costs **more** per unit (its ns value is higher). The marginals
(`*_ns_*`) are the load-robust cross-runtime signal; the wall/startup rows are inflated by the load
above and are shown for completeness.

| workload | headline cross-runtime metric | sys | rust | rust/sys |
|---|---|---|---|---|
| `baseline`   | `startup_ms` (mean, whole-process) | 126.5878 ms | 17.2648 ms | **0.136×** (rust 7.33× faster) |
| `baseline`   | `vm_memory_total_bytes` | 36 205 168 B | 805 458 B | **0.022×** |
| `slice`      | `case_<name>_wall_ms` (mean, per case) | 128.47–139.99 ms | 17.27–41.29 ms | **0.123×–0.309×** (rust faster) |
| `foreign`    | `case_<name>_ns_per_call` | 222.49–371.84 ns | 10 283.30–24 782.40 ns | **46.2×–74.2×** (rust higher) |
| `list_walk`  | `case_list_walk_ns_per_element` | 2.4238 ns | 158.8297 ns | **65.5×** (rust higher) |
| `copy_bound` | `case_<name>_ns_per_op` | 239.95–1 450.39 ns | 3 391.36–165 897.30 ns | **14.1×–118.5×** (rust higher) |
| `gc_bound`   | `case_churn_ns_per_op` | 4 248.40 ns | 179 192.65 ns | **42.2×** (rust higher) |

### 5.1 Per-case detail

`baseline` (whole-process cost — startup latency is the trustworthy cross-runtime comparison):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `startup_ms` (mean) | 126.5878 | 17.2648 | 0.136× |
| `compute_loop_ms` (mean) | 204.6710 | 58.2682 | 0.285× |
| `compute_ops_per_sec` | 4 885 888.92 | 17 162 012.22 | 3.51× (rust faster; native floor, not interpreter throughput) |
| `vm_memory_total_bytes` | 36 205 168 | 805 458 | 44.95× |
| `peak_memory_bytes` (RSS) | 135 938 048 | 31 723 520 | 4.29× |

`slice` (one call per process: `case_<name>_wall_ms` mean, and the comparable per-process memory):

| case | sys wall_ms | rust wall_ms | rust/sys |
|---|---|---|---|
| `add` | 138.0101 | 18.7426 | 0.136× |
| `fib` | 135.9342 | 18.0635 | 0.133× |
| `pick` | 139.9908 | 17.2723 | 0.123× |
| `calendar_last_day` | 137.9494 | 18.7622 | 0.136× |
| `queue_from_size` | 138.7089 | 22.3967 | 0.162× |
| `gb_trees_size` | 139.2973 | 18.1340 | 0.130× |
| `base64_encode_size` | 130.4474 | 19.4397 | 0.149× |
| `ordsets_from_size` | 128.4741 | 18.0949 | 0.141× |
| `string_str_length` | 133.5997 | 41.2937 | 0.309× |

`slice` process `peak_memory_bytes`: sys 72 105 984 → rust 27 660 288 (**2.61×** lower for rust).

`foreign` (real Cowboy + EMQX modules; startup-cancelled `ns_per_call`):

| case | sys ns/call | rust ns/call | rust/sys |
|---|---|---|---|
| `bstr_to_lower` | 371.8393 | 19 606.7325 | 52.73× |
| `bstr_capitalize_token` | 334.0372 | 24 782.3962 | 74.19× |
| `emqx_base62_encode` | 222.4924 | 10 283.3025 | 46.22× |
| `emqx_base62_decode` | 309.4556 | 17 698.4921 | 57.19× |

`list_walk` (BIF-free interpreted list walk; startup-cancelled per element):

| metric | sys | rust |
|---|---|---|
| `case_list_walk_ns_per_element` | 2.4238 ns | 158.8297 ns (**65.53×** higher) |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 |

`copy_bound` (ETF / ETS / send-receive copy boundaries; startup-cancelled `ns_per_op`):

| case | op | sys ns/op | rust ns/op | rust/sys | ops sys / rust (deterministic) |
|---|---|---|---|---|---|
| `etf_roundtrip` | `term_to_binary`/`binary_to_term` | 650.9391 | 77 137.9386 | 118.50× | 1 500 000 / 5 000 |
| `ets_copy` | `ets:insert`+`ets:lookup` | 239.9495 | 3 391.3562 | 14.13× | 2 000 000 / 100 000 |
| `send_recv` | two-process send/receive copy | 1 450.3917 | 165 897.2982 | 114.38× | 500 000 / 2 000 |

`gc_bound` (allocate-and-abandon churn; startup-cancelled `ns_per_op`):

| metric | sys | rust |
|---|---|---|
| `case_churn_ns_per_op` | 4 248.3978 ns | 179 192.6512 ns (**42.18×** higher) |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 |

> `gc_bound`'s row measures the allocate-and-abandon **wall time**; whether a runtime's collector
> actually runs during the loop is its own admission policy and is not claimed here (the report's
> own note says the same).

## 6. Sys-only external targets (`m9-bench-external.json`, `--full`)

| workload | status | headline |
|---|---|---|
| `erlc_compiler` | `ok` | wall 6 094.92 ms; 9.844 files/s (60 produced over 46 503 LOC) |
| `elixir_compiler` | `ok` | wall 48 696.61 ms; 4.579 files/s (105 attempted / 223 produced) |
| `cowboy` | `ok` | 220 143.53 req/s; p50 0.0639 ms / p99 0.2764 ms (1 101 086 requests, 0 errors) |
| `khepri` | `ok` | `rebar3 eunit` wall 77 755.91 ms |
| `plb` (`m9-bench-plb.json`) | `failed(plb-build-failed)` | `NETSDK1045` — `PATH` `dotnet` 8.0.425 vs the `net9.0` target; no number |
| `emqx` (`m9-bench-emqx.json`) | `skipped(emqx-requires-otp-28)` | no number |

## 7. Honesty check

- Rust rows: all `ok` with real metrics; none `unsupported`, none fabricated, no workload crashed.
- `sys`/`rust` use identical source programs (embedded modules compiled once with the system `erlc`);
  each run's stdout was checked against the deterministic constant on both sides.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` figures are **startup-cancelled** marginals
  `(mean wall@N − mean wall@0) / N`, so per-process startup (including the Rust runtime's code-path
  resolution) cancels on both sides.
- `plb` and `emqx` are the only non-`ok` rows, and both carry a verified reason and **no** number.
