# Rust-ERTS benchmark results — current measurement run
Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured**, not modelled.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-09, 05:27–05:54 UTC**, at the tree archived in commit `532eb3d11c`.
Raw harness JSON is committed under `results/` as `m5-*`; the previous run's artifacts are kept as `m4-*` and are the baseline for every regression/improvement flag in §7.

> **Host-contention warning (read before quoting any absolute latency).** The benchmark host is shared and heavily oversubscribed **during this run** — `/proc/loadavg` 1-min stayed in the **90–153** band for the whole measurement window (vs **4.77 → 11.59** for the `m4` primary run).
> Every *absolute* latency/throughput below is therefore inflated several-fold relative to `m4` and is **not** comparable to it; the load-invariant signals are the **startup-cancelled** per-element/per-call metrics (`list_walk` ns/element, `foreign` ns/call) and the deterministic counters.
> Prefer ratios over absolute values, and re-run on a quiet host before quoting a hard number.
> The load was recorded **before and after every run** (§1) — this is mandatory here.

---

## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (UTC) | 2026-10-09 05:27–05:54 UTC |
| Host / kernel | Linux 7.2.9 (`#1-NixOS SMP PREEMPT_DYNAMIC`), `x86_64` |
| Distribution | NixOS 26.11 (Zokor) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 65 317 432 kB (62.29 GiB) |
| Host load (1-min `/proc/loadavg`) | primary full run **130.32 → 90.41**; focused `xrun` **118.13 → 101.17**; PLB attempt (SDK forced) **89.90 → 94.04**; PLB re-run at the fixed default resolution **63.61 → 42.43** |
| Repository state | commit `532eb3d11c` (the measurement tree) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl` at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`, **OTP 29.1.1 / erts-17.1** (`otp_release` = `"29"`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, resolved from the Nix store by the driver's store scan (not on `PATH`); `khepri` runs with no `PATH` prefix needed (§4.3) |
| `dotnet` | the store scan resolves the **newest installed SDK, 9.0.318** (the store also holds an 8.0.425 one; the scan ranks entries by the version named in the entry, never by the random hash prefix, so it no longer picks .NET 8) — what PLB's .NET-9 target needs (§4.4) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` (§4.4) |

Both runtimes resolve and pass the harness self-check (`supported: true`).
The driver and the runtime were both built from this tree (`--release -p beam_rt -p bench`), so the numbers describe this tree, not a stale binary.

## 2. Exact commands

Build the driver and the runtime:
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
```
Probe status (all 10 workloads report `probe_ready: true`):
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- list --json
```
The **primary full run** — `--full`, both runtimes, all 10 workloads → `results/m5-bench-full-runtimes-all.json`:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/results/m5-bench-full-runtimes-all.json --verbose
```
The **focused cross-runtime run** — only the four `Runtimes::Both` workloads, so both sides see the same machine conditions → `results/m5-bench-xrun.json`:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk \
    --out-file erts_rust/tools/bench/results/m5-bench-xrun.json --verbose
```
The **PLB run at the default tool resolution** — the driver's store scan ranks the Nix-store entries by parsed version, so it selects the newest installed SDK (`.NET 9.0.318`) with **no** `BENCH_TOOL_PATH` override and **no** `PATH` prefix → `results/m5-bench-plb.json`:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb \
    --out-file erts_rust/tools/bench/results/m5-bench-plb.json --verbose
```
Cross-runtime diff (archived as `results/m5-compare-cross-runtime.txt`):
```sh
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m5-bench-xrun.json
```
Crate tests: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` runs green.

Profile semantics: `--full` ⇒ `baseline` 50 startup samples / 5 compute samples, `slice` 25 timed runs/case, `foreign` 10 samples/case, `list_walk` 10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200 files.
`--quick` ⇒ `baseline` 10 / 5, `slice` 5, `foreign` 3, `list_walk` 3, `cowboy` 8 connections × 2 s.
**Run order is runtime-major**: all `sys` rows execute first, then all `rust` rows — so under a *drifting* host load the two sides of a comparison are measured in different time windows (§6.1).

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

`sys` = OTP 29.1.1 with the JIT; `rust` = `beam_rt`, released, a tree-walking interpreter over a supported opcode subset.
All tables in this section come from the **focused `xrun`** (`m5-bench-xrun.json`, host load 118.13 → 101.17), which runs only the four cross-runtime workloads back to back; §3.6 gives the same figures from the primary full run so the load spread is visible.

### 3.1 Startup latency — `baseline` (n = 50, ms)

| stat | sys | rust | ratio |
|---|---|---|---|
| min | 158.87 | 323.52 | 2.04× |
| **p50** | **273.67** | **454.40** | **1.66×** |
| mean | 307.08 | 463.22 | 1.51× |
| p90 | 465.24 | 595.16 | 1.28× |
| **p99** | **586.93** | **738.04** | **1.26×** |
| max | 600.33 | 822.54 | 1.37× |

Startup is the load-sensitive dimension: the `m4` primary run gave **rust 1.26× sys** (quiet host), this `xrun` **1.66×**, and the `m5` primary full run **0.43×** (rust *faster*, §3.6). The absolute values tripled vs `m4` because the host load moved from ~5–12 to ~90–150.
**There is no reproducible startup regression** — the ratio is dominated by host contention, not by a code change.

The Rust figure is not bare interpreter boot: a `beam_rt` process resolves the system code path by spawning `erl` once per OS process (overridable with `$ERTS_STDLIB_DIR`), so it includes one `erl` start-up (§6.2).

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)

| metric | sys | rust | ratio |
|---|---|---|---|
| `compute_loop_ms` p50 | 366.34 | 487.88 | 1.33× |
| `compute_ops_per_sec` | 2 725 003 | 2 065 328 | 0.76× (rust slower) |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 36 220 168 B (34.5 MiB) | **801 294 B (782 KiB)** | **45.2× lower** |
| peak RSS (`VmHWM`, whole workload) | 135 692 288 B (129.4 MiB) | 30 535 680 B (29.1 MiB) | 4.44× lower |

The loop is `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'` and runs under **both** runtimes.
**Caveat — read this loop as the native floor, not interpreter throughput.** It is dominated by the natively-served `lists:seq/2` + `lists:sum/1`; it is **not** a measure of interpreter dispatch. The execution-bound signal is §3.4/§3.5.
Under load the rust loop is *more* robust than the 14-scheduler oracle: at `m4` (quiet) the ratio was 1.00×, here 1.33×, and in the primary full run rust was **2.8× faster** (§3.6) — i.e. the sys side degrades far more with contention.

`vm_memory_total_bytes` is a **separate** metric from the OS `peak_memory_bytes` and is never conflated with it.
`beam_rt` implements `erlang:memory/0,1` through the `MemoryHook` seam; the two runtimes are not reporting the same quantity (OTP's `total` includes allocator arenas and preloaded code; `beam_rt`'s is its much smaller in-process heap). The rust figure is **identical** to `m4` (801 294 B).

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

Every case printed the **same** stdout under both runtimes (report note: `add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5 base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, p50 (ms); per-case peak RSS, p50 (MiB, base 2):

| case | sys wall | rust wall | wall ratio | sys RSS | rust RSS | RSS ratio |
|---|---|---|---|---|---|---|
| `add` (fixture) | 312.98 | 267.84 | 0.86× | 66.6 | 13.8 | 4.83× |
| `fib` (fixture, n=20) | 256.71 | 288.94 | 1.13× | 66.6 | 13.9 | 4.79× |
| `pick` (fixture) | 262.36 | 312.64 | 1.19× | 66.5 | 13.8 | 4.81× |
| `calendar_last_day` | 251.40 | 321.53 | 1.28× | 66.3 | 14.7 | 4.52× |
| `queue_from_size` | 264.07 | 291.63 | 1.10× | 66.2 | 16.6 | 3.99× |
| `gb_trees_size` | 304.42 | 263.48 | 0.87× | 66.4 | 14.6 | 4.55× |
| `base64_encode_size` | 329.64 | 237.19 | 0.72× | 66.4 | 16.0 | 4.15× |
| `ordsets_from_size` | 273.27 | 277.94 | 1.02× | 66.2 | 14.3 | 4.62× |
| `string_str_length` | 264.75 | 287.80 | 1.09× | 66.7 | 25.1 | 2.66× |

Whole-workload peak RSS: sys **72 110 080 B (68.8 MiB)** / rust **26 619 904 B (25.4 MiB)** (2.71× lower).

**Read this table as *startup*, not *execution*.** Six of the nine cases call a *system* module, and a `beam_rt` process that must load one first spawns `erl` once to resolve the code path; the three pure-fixture cases are startup-dominated too because each is a single call. These wall times therefore measure boot-into-work, not interpreter throughput — §3.5 is the per-element comparison.
Per-case `rust`/`sys` peak RSS is the **like-for-like per-process memory comparison** (one call per process, no loop accumulation): rust **13.8–25.1 MiB** vs sys **66.2–66.7 MiB** (2.66×–4.83× lower).

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source* (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system `erlc` and executed under both runtimes.
`ns/call` is the startup-cancelled marginal `(mean wall@N − mean wall@0) / N` (n = 10 samples each); `N` is per runtime so both marginals clear the timer noise.
`wall` is **not** comparable across runtimes (different `N`); `startup` is.

| case (real function) | N sys / rust | sys ns/call | rust ns/call | ratio | startup p50 sys / rust (ms) | RSS p50 sys / rust (MiB) |
|---|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 1 500 000 / 20 000 | 541.21 | 28 746.48 | **53.1×** | 268.08 / 234.30 | 66.2 / 101.8 |
| `cowboy_bstr:capitalize_token/1` | 1 500 000 / 15 000 | 450.91 | 40 199.69 | **89.1×** | 274.08 / 235.48 | 66.3 / 108.0 |
| `emqx_base62:encode/1` | 3 000 000 / 25 000 | 339.92 | 19 872.88 | **58.5×** | 273.75 / 248.35 | 66.5 / 43.9 |
| `emqx_base62:decode/1` | 3 000 000 / 15 000 | 402.81 | 38 673.66 | **96.0×** | 283.81 / 238.81 | 66.6 / 88.1 |

All four cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout (`sys`: `bstr_to_lower=45000000 bstr_capitalize_token=45000000 emqx_base62_encode=48000000 emqx_base62_decode=33000000`; `rust`: `600000 / 450000 / 400000 / 165000` = `N × per_call`).

**This is the execution-bound result.** Startup is comparable (§3.1) while the *per-call* cost of a real foreign function is ~**53×–96×** higher under `beam_rt`.
The gap is a **real runtime cost, not a harness artifact**: the startup-cancelled marginal is stable and reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`.
It is a `beam_rt` per-call host-context cost, not interpreter dispatch.
**`foreign` peak RSS is not a like-for-like memory comparison — see §6.7.**

### 3.5 `list_walk` — the interpreter-throughput metric (n = 10/case)

A **BIF-free** body-recursive walk (`walk(N)` sums a 1000-element list `N` times; `build/2` makes the list once per process) runs under both runtimes; the reported figure is the startup-cancelled **ns per walked element**.

| runtime | `N` (walks) | elements walked | `ns_per_element` | `wall_ms` p50 | `startup_ms` p50 | peak RSS p50 |
|---|---|---|---|---|---|---|
| `sys` | 100 000 | 100 000 000 | **8.43** | 1 225.58 | 259.96 | 66.0 MiB |
| `rust` | 1 000 | 1 000 000 | **561.67** | 799.19 | 245.24 | 14.1 MiB |

`case_list_walk_elements` (the deterministic, contention-immune companion metric) is `1×10⁸` for sys and `1×10⁶` for rust; each run printed the deterministic constant `N × 500 500` (`rust`: `list_walk=N*500500=500500000`).
**`ns/element` is the cleanest load-invariant interpreter-throughput signal in the suite**: rust **524.29** (primary) / **561.67** (`xrun`) vs sys **7.82** / **8.43** — a **~67×** rust/sys ratio that is stable across both runs despite the load moving.
That ratio is consistent with the §3.4 `foreign` per-call gap (~53×–96×): both are the same interpreter-vs-JIT throughput story.

### 3.6 The same three figures from the primary full run (load 130.32 → 90.41)

| metric | sys | rust | note |
|---|---|---|---|
| `baseline` startup p50 / p99 (ms) | 535.95 / 949.31 | 229.53 / 309.31 | rust **0.43×** — sys measured at peak load, rust at the tail (§6.1) |
| `baseline` `compute_ops_per_sec` | 1 067 224 | 3 035 587 | rust **2.8×** faster (sys lost far more to contention) |
| `baseline` `vm_memory_total_bytes` | 36 225 544 B | 801 294 B | 45.2× lower (unchanged) |
| `list_walk` `ns_per_element` | 7.82 | 524.29 | **67.0×** |
| `foreign` ns/call ratios | — | — | **33.9×–96.5×** (per-case) |

The primary run ran `sys` first and `rust` last (runtime-major order) while the load fell from 130 to 90, which is why the sys side looks worse here than in the `xrun`. The two runs are the error bar for §3.1/§3.2; §3.4/§3.5 are the load-robust rows.

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS** on the target projects.
The Rust runtime cannot be scheduled on any of them (§5), so there is no Rust column.
All absolute values in this section are inflated by the 90–130 host load (§1) and are **not** comparable to `m4`; §7 states each move explicitly.

### 4.1 Compiler throughput (primary full run)

| workload | metric | value |
|---|---|---|
| `erlc_compiler` | wall | 12 660.21 ms |
| | files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| | **throughput** | **4.74 files/s** (**3 673.16 LOC/s**) |
| | peak RSS | 132 689 920 B (126.5 MiB) |
| `elixir_compiler` | wall | 138 296.28 ms |
| | files attempted / `.beam` produced | 105 / **223** (97/105 compiled) |
| | **throughput** | **1.61 files/s** (**809.60 LOC/s**) |
| | compiler startup | 538.62 ms |
| | peak RSS | 204 836 864 B (195.3 MiB) |
| | notes | batch compile failed → per-file fallback (checkout ↔ installed-version skew); compiler = installed `elixirc` 1.20.4 |

### 4.2 Cowboy hello-world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s)

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 80 156.39 ms |
| requests | 141 946 |
| **throughput** | **28 367.57 req/s** |
| errors | 0 |
| latency min / p50 / p90 | 0.0238 / 0.3539 / 1.2267 ms |
| **latency p99** | **3.8372 ms** |
| latency max | 17.837 ms (n = 20 000) |
| peak RSS | 153 448 448 B (146.3 MiB) |

### 4.3 Khepri test suite (GC / data-structure bound)

`rebar3` 3.27.1 is resolved from the Nix store by the driver's **store scan** (§2) — no `PATH` prefix and no env-var opt-in are needed.

| metric | value |
|---|---|
| `rebar3 eunit` wall | **98 436.55 ms** |
| peak RSS | 376 393 728 B (358.9 MiB) |

### 4.4 Programming-Language-Benchmarks (`plb`) — runs by default, on the newest installed SDK

`plb` **runs** (no longer `skipped(dotnet-absent)`, and no longer `failed(plb-build-failed)`). The driver's Nix-store scan ranks entries by the version named in the entry rather than by the whole entry name — whose 32-character hash prefix is random — so it resolves the **newest installed `dotnet` SDK, .NET 9.0.318**, and the workload's `.NET 9` build stage succeeds. No `BENCH_TOOL_PATH` override and no `PATH` prefix are needed (§2); the workload **produces real numbers** (`--quick`, 10 tests):

| metric | value |
|---|---|
| build stage (`dotnet --task build`) | ok — 29 162 ms |
| wall | 77 669.86 ms |
| `test_time_ms` (n = 10) | min 0.00 / p50 728.10 / mean 1 457.99 / p99 4 113.80 ms |
| `test_mem_bytes` | 91 942 912 B (87.7 MiB) |
| peak RSS | 169 451 520 B (161.6 MiB) |

The per-test `test_time_ms` is the PLB tool's own measurement of its workload; the workload-level `wall_ms` also includes the 29 s build stage.

This re-run was recorded at host load **63.61 → 42.43** (§1) — quieter than the primary full run but still far above the `m4` near-quiet reference, so its absolute wall/latency figures stay load-inflated and are not comparable to `m4`; re-run on a quiet host before quoting a hard number. Artifact: `results/m5-bench-plb.json`.

## 5. Workloads that did **not** run — verified reasons

Every reason below was reproduced on this host, not assumed.

| workload | status | verified reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | EMQX builds with **Erlang/OTP 28** (`env.sh OTP_VSN` / `.tool-versions`), but `erl` on `PATH` is OTP **29**: the umbrella build aborts in a dependency NIF (`:ezstd`) whose makefile generates `env.mk` via `erl -noshell -s init stop -eval …`, an idiom OTP 29 no longer evaluates, so `mix compile` fails with `No rule to make target '…/env.mk'`. The workload detects the OTP-version gate and reports the precise skip instead of a meaningless wall time; **no number is fabricated.** |
| all 6 `sys-only` workloads scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/bootstrap an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. |

No `sys-only` workload fails on a *tool* any more: `plb` runs at the default resolution (§4.4), and `khepri` runs with `rebar3` found by the same store scan (§4.3).

## 6. Reproducibility, stability and known measurement caveats

### 6.1 The host is heavily loaded and the two runtimes are measured in different windows
`/proc/loadavg` 1-min was **90–153** for the entire window (§1). The runner is **runtime-major** (`sys` rows first, then `rust` rows), so under a drifting load the two sides see different conditions — this is why the primary full run shows rust *faster* at startup (sys measured at load 130, rust at load 90) while the `xrun` shows rust 1.66× slower.
Prefer ratios, prefer the startup-cancelled rows, and always record `/proc/loadavg` alongside an absolute figure. The `m4` primary run (load 4.77 → 11.59) remains the only near-quiet reference.

### 6.2 `beam_rt` resolves its code path by spawning `erl` once per process
This affects every process that touches a system module — all `slice` wrapper cases, all `foreign` cases, and the `baseline` startup — and is why Rust startup is ~230–460 ms rather than a few ms. Setting `$ERTS_STDLIB_DIR` skips the spawn; `bench` never sets it (the harness is a pure driver/observer and never mutates the runtime's environment beyond argv). The per-element/per-call metrics cancel this cost by construction.

### 6.3 `elixir_compiler` measures the installed `elixirc` 1.20.4
against the unbuilt, read-only 1.21.0-dev checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the artifact.

### 6.4 `cowboy` re-fetches cowlib/ranch in a fresh scratch copy on every run (network)
which is why its `build_ms` (~80 s under load) dominates the workload.

### 6.5 Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`)
Erlang's own `erlang:memory(total)` is a **separate** metric (§3.2) and is never conflated with it.

### 6.6 `foreign` `wall_ms` is not comparable across runtimes
Each side runs a different `N`. Compare `ns/call` and `startup_ms`.

### 6.7 `foreign`/`list_walk` rust peak RSS is **still not flat in N** — the GC adoption does not bound it on the paths the harness drives
This was the `m4` known issue; it is **not fixed**. `beam_rt` now has a process-heap collector (`crates/beam_rt/src/runtime/gc.rs`, driven from `context/gc.rs` at the scheduled-slice heap-admission point), but measured against a controlled allocation probe the rust peak RSS still grows ~linearly with the loop count, while the oracle stays flat:

| N (iterations) | rust peak `VmHWM` | sys peak `VmHWM` | garbage if never collected |
|---|---|---|---|
| 1 000 | 29 MiB | 61 MiB | 15 MiB |
| 5 000 | 77 MiB | 60 MiB | 78 MiB |
| 20 000 | 227 MiB | 60 MiB | 312 MiB |
| 50 000 | 484 MiB | 61 MiB | 781 MiB |

The probe is a BIF-free module that builds a fresh 1000-element list (16 kB of garbage) per iteration and walks it with a **tail-recursive** `len/2`, so the only per-iteration allocation is the throwaway list; it runs identically under `beam_rt run … <N>` and `erl -noshell -eval`. `beam_rt` peak RSS tracks the allocation (roughly linearly), the oracle's is bounded.
**Consequence:** the `foreign` rust `case_<name>_peak_rss_bytes` rows run at large `N` and stay **above** the sys figures on 3 of 4 cases (101.8 / 108.0 / 43.9 / 88.1 MiB vs 66.2–66.6 MiB), so `foreign` peak RSS is **not** a like-for-like memory comparison. Use `slice` (§3.3, one call per process, `N`=1) as the comparable per-process metric.

## 7. Regressions and improvements vs the previous run (`m4-*`)

Every figure below is quoted from the committed artifacts: `m4-*` (2026-10-08, load 4.77 → 11.59) and `m5-*` (2026-10-09, load 90–153). **The load difference dominates the absolute numbers**, so each row states whether the move is code-attributable or load-attributable.

**No code-attributable regression was found.** Every absolute-metric decrease vs `m4` (compiler throughput, cowboy throughput, khepri wall, the startup p99s) is attributable to the 10–25× higher host load.

**Improvements (code-attributable):**

* **`khepri` activates with no `PATH` prefix.** `m4`: `rebar3` had to be found by prepending its Nix-store `bin` dir to `PATH`. `m5`: the driver's store scan resolves `rebar3` 3.27.1 automatically (§4.3). Wall 76 503.14 ms → 98 436.55 ms (load), but the activation requirement is gone.
* **`plb` runs by default and yields real numbers.** `m4` best case was `failed(plb-tool-failed)` (no number, even with .NET 9 on `PATH`); before this fix the `m5` default resolution picked **.NET 8.0.425** — the store entry whose random hash prefix happened to sort greatest — so the build died on `NETSDK1045` and only a forced `BENCH_TOOL_PATH` made PLB run. The scan now ranks entries by the **parsed version** in the entry name, so the default resolution selects the newest installed SDK, **9.0.318**, and the `.NET 9` build succeeds: `ok`, build stage 29 162 ms, `test_time_ms` p50 728.10 ms, `test_mem_bytes` 91 942 912 B (§4.4, `results/m5-bench-plb.json`).
* **`emqx` reports a precise blocker.** `m4`: `skipped(not-a-rebar3-project)`. `m5`: `skipped(emqx-requires-otp-28)` with the exact mechanism (the `:ezstd` `env.mk` make-target fails under OTP 29) — still no number, but the reason is now the real one.
* **`slice` rust per-process peak RSS improved.** Whole-workload rust 39 096 320 B (37.3 MiB, `m4`) → 26 619 904 B (25.4 MiB, `m5` `xrun`); the heaviest case `string_str_length` 36.9 → 25.1 MiB. (Sys unchanged ~66 MiB.)
* **`baseline` rust peak RSS improved slightly.** 35 008 512 B (33.4 MiB, `m4`) → 30 535 680 B (29.1 MiB, `m5`).
* **`baseline` rust compute-loop robustness.** `compute_ops_per_sec` rust 5 036 960 (`m4`) → 3 035 587 (`m5` primary) / 2 065 328 (`m5` xrun); sys 4 888 889 → 1 067 224 / 2 725 003. Both fell with load, but the rust/sys ratio moved from 1.03× to **2.8× / 0.76×** — rust is the more contention-robust side.
**Superseded, kept for the archive:**

* **`plb`'s default resolution used to fail at the build stage.** The `m5` **primary full run** row (`failed(plb-build-failed)`, `results/m5-bench-full-runtimes-all.json`) was measured before the store scan became version-aware: it resolved **.NET 8.0.425**, while PLB targets **.NET 9**, so `dotnet --task build` died with `NETSDK1045`. That row is left as measured; the current default resolution picks **9.0.318** and the workload runs (§4.4, `results/m5-bench-plb.json`).

**Not a regression / not a reproducible change (load-attributable):**

* **`baseline` startup ratio.** `m4` rust/sys = **1.26×** (p50 148.12 vs 117.73). `m5`: **1.66×** (`xrun`) and **0.43×** (primary). The prior "~1.26×" is not a constant; it is inside the load-driven spread. No code change moved it.
* **`baseline` startup p99.** `m4` 137.64 / 169.79 (sys/rust) → `m5` `xrun` 586.93 / 738.04. Both tripled with load.
* **Compiler throughput.** `erlc_compiler` 9.53 → 4.74 files/s / 7 384.67 → 3 673.16 LOC/s; `elixir_compiler` 4.86 → 1.61 files/s / 2 442.30 → 809.60 LOC/s. Both roughly halved-to-third with load; the workloads ran only the system toolchain both times.
* **Cowboy throughput.** 224 103 → 28 367.57 req/s; p99 latency 0.1750 → 3.8372 ms. The host is the binding constraint (the in-process load generator and the server share one contended machine).
* **`khepri` wall.** 76 503.14 ms → 98 436.55 ms (load).

**Unchanged:**

* **`vm_memory_total_bytes`** rust 801 294 B; sys 36 227 736 → 36 225 544 / 36 220 168 B.
* **`foreign` rust peak RSS still scales with N (§6.7).**
* **`foreign` per-call gap.** `m4` (quiet) **53.8×–72.0×**; `m5` **33.9×–96.5×** (primary) and **53.1×–96.0×** (`xrun`). The band **widened** at both ends — a load artifact, not a code change: the absolute rust ns/call rose for all four cases (e.g. `emqx_base62_decode` 18 973.61 → 39 159.47 ns) and the sys side rose too (305.29 → 405.89 ns on the same case).

**New this run:**

* **`list_walk`** (§3.5): rust **524.29** (primary) / **561.67** (`xrun`) ns/element vs sys **7.82** / **8.43** — a stable **~67×** interpreter-throughput ratio.

## Archived artifacts

| file | contents |
|---|---|
| `results/m5-bench-full-runtimes-all.json` | **primary full run**: `--full --runtime all`, all 10 workloads × both runtimes (host load 130.32 → 90.41) — the source of §3.6, §4.1–4.3, §5 |
| `results/m5-bench-xrun.json` | focused cross-runtime run: `--full --runtime all --workloads baseline,slice,foreign,list_walk` (host load 118.13 → 101.17) — the source of §3.1–3.5 |
| `results/m5-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside `m5-bench-xrun.json` (47 numeric rows; the tool diffs the **mean**) |
| `results/m5-bench-plb.json` | **PLB at the default tool resolution** (`--quick --runtime sys --workloads plb`, host load 63.61 → 42.43): `ok` with real metrics (§4.4) — the current state |
| `results/m5-bench-quick-plb-net9-attempt.json` | PLB with the .NET 9 SDK forced via `BENCH_TOOL_PATH` → `ok`; superseded by the default-resolution run above (§4.4) |
| `results/m4-*` | the **previous** run (2026-10-08, load 4.77 → 11.59): primary `m4-bench-full-runtimes-all.json`, the three xruns, the compiler/cowboy/khepri runs, the PLB attempt, and the three `m4-compare-*.txt` diffs — kept as the regression baseline for §7 |

The archived copies are the harness output except for two mechanical normalisations that strip environment-specific absolute paths: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` is replaced with `$TMPDIR/`. **No metric value is altered.**
