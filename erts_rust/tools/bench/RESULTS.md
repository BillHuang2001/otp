# Rust-ERTS benchmark results — current measurement run
Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured**, not modelled.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-08, 17:33–17:49 UTC** (2026-10-09, 01:33–01:49 HKT), at the tree archived in commit `c320a839e9`.
Raw harness JSON is committed under `results/` as `m4-*`.

> **Host-contention warning (read before quoting any absolute latency).** The benchmark host is shared and oversubscribed.
> The headline cross-runtime tables (§3) come from the single least-contended full run (`results/m4-bench-full-runtimes-all.json`, `/proc/loadavg` 4.77 → 11.59).
> The three focused `baseline,slice,foreign` runs in `results/m4-bench-xrun-*.json` were taken under heavy contention (`/proc/loadavg` ~22–37) and their absolute latencies are inflated several-fold; they exist only as the reproducibility error bar (§6).
> Prefer ratios over absolute values, and re-run on a quiet host before quoting a hard number.

---

## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (local / UTC) | 2026-10-09 01:33–01:49 HKT / 2026-10-08 17:33–17:49 UTC |
| Host / kernel | Linux 7.2.9 (`#1-NixOS SMP PREEMPT_DYNAMIC`), `x86_64` |
| Distribution | NixOS 26.11 (Zokor) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 65 317 432 kB (62.29 GiB) |
| Host load | `/proc/loadavg` 1-min **4.77 → 11.59** during the primary full run; ~22–37 during the focused `xrun` runs (shared host) |
| Repository state | commit `c320a839e9` (the measurement tree) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl` at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`, **OTP 29.1.1 / erts-17.1** (`otp_release` = `"29"`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, built in the Nix store (not on `PATH` by default); `khepri` runs once its `bin` dir is prepended to `PATH` (§2, §5) |
| `dotnet` | **9.0.318**, built in the Nix store (not on `PATH` by default); PLB still yields no number (§5) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` (§5) |

Both runtimes resolve and pass the harness self-check (`supported: true`).
The driver and the runtime were both built from this tree (`--release -p beam_rt -p bench`), so the numbers describe this tree, not a stale binary.

## 2. Exact commands

Build the driver and the runtime:
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
```
Probe status:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- list --json
```
The **primary full run** — `--full`, both runtimes, all 9 workloads → `results/m4-bench-full-runtimes-all.json`:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/out/milestone-full.json --verbose
```
The **focused cross-runtime runs** — the three cross-runtime workloads only, so both sides see the same machine conditions → `results/m4-bench-xrun-baseline-slice-foreign.json` (+ `-run2`, `-run3`):
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign \
    --out-file erts_rust/tools/bench/out/xrun-full.json --verbose
```
The **compiler runs** → `results/m4-bench-full-compilers-sys.json` (+ `-run2`):
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler \
    --out-file erts_rust/tools/bench/out/compilers-sys.json --verbose
```
The **cowboy run** → `results/m4-bench-full-cowboy-sys.json`:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads cowboy \
    --out-file erts_rust/tools/bench/out/cowboy-sys.json --verbose
```
The **khepri/emqx run** — `rebar3` (3.27.1, built against OTP 28) is found by prepending its Nix-store `bin` dir to `PATH` (no env-var opt-in) → `results/m4-bench-full-khepri-emqx.json`:
```sh
PATH=/nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin:$PATH \
  erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads khepri,emqx \
    --out-file erts_rust/tools/bench/out/khepri-emqx.json --verbose
```
The **PLB attempt** — `dotnet` 9.0.318 found by prepending its Nix-store `bin` dir to `PATH` → `results/m4-bench-quick-plb-net9-attempt.json`:
```sh
PATH=/nix/store/g6lfw3lizv0ayzy7yhq5g0rk9swxysii-dotnet-sdk-wrapped-9.0.318/bin:$PATH \
  erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb \
    --out-file erts_rust/tools/bench/out/plb-net9-attempt.json --verbose
```
Comparisons (archived as `results/m4-compare-*.txt`):
```sh
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m4-bench-full-runtimes-all.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m4-bench-xrun-baseline-slice-foreign.json \
                                       erts_rust/tools/bench/results/m4-bench-xrun-baseline-slice-foreign-run2.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m4-bench-xrun-baseline-slice-foreign-run2.json \
                                       erts_rust/tools/bench/results/m4-bench-xrun-baseline-slice-foreign-run3.json
```
Crate tests: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` runs green.

Profile semantics: `--full` ⇒ `baseline` 50 startup samples / 5 compute samples, `slice` 25 timed runs/case, `foreign` 10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200 files.
`--quick` ⇒ `baseline` 10 / 5, `slice` 5, `foreign` 3, `cowboy` 8 connections × 2 s.

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

`sys` = OTP 29.1.1 with the JIT; `rust` = `beam_rt`, released, a tree-walking interpreter over a supported opcode subset.
`ratio` = rust / sys (values > 1 mean Rust is slower).
All tables in this section come from the primary full run `m4-bench-full-runtimes-all.json` (§1 host load 4.77 → 11.59).

### 3.1 Startup latency — `baseline` (n = 50, ms)

| stat | sys | rust | ratio |
|---|---|---|---|
| min | 104.27 | 130.79 | 1.25× |
| **p50** | **117.73** | **148.12** | **1.26×** |
| mean | 118.93 | 147.82 | 1.24× |
| p90 | 129.52 | 160.66 | 1.24× |
| **p99** | **137.64** | **169.79** | **1.23×** |
| max | 140.58 | 173.88 | 1.24× |

Startup is the one dimension where the two runtimes are comparable (~1.25×).
The Rust figure is not bare interpreter boot: a `beam_rt` process resolves the system code path by spawning `erl` once per OS process (overridable with `$ERTS_STDLIB_DIR`), so it includes one `erl` start-up (§6).

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)

| metric | sys | rust | ratio |
|---|---|---|---|
| `compute_loop_ms` p50 | 198.06 | 197.03 | 1.00× |
| `compute_ops_per_sec` | 4 888 889 | 5 036 960 | 1.03× (rust faster) |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 36 227 736 B (34.5 MiB) | **801 294 B (782 KiB)** | **45.2× lower** |
| peak RSS (`VmHWM`, whole workload) | 134 799 360 B (128.6 MiB) | 35 008 512 B (33.4 MiB) | 3.85× lower |

The loop is `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'` and runs under **both** runtimes.
**Caveat — read this loop as the native floor, not interpreter throughput.** It is dominated by the natively-served `lists:seq/2` + `lists:sum/1`, so a rust figure at parity says the native list floor is fast; it is **not** a measure of interpreter dispatch or of the per-call cost that `foreign` (§3.4) exposes.

`vm_memory_total_bytes` is a **separate** metric from the OS `peak_memory_bytes` and is never conflated with it.
`beam_rt` implements `erlang:memory/0,1` through the `MemoryHook` seam, so it returns a real VM-reported figure; the two runtimes are not reporting the same quantity (OTP's `total` includes allocator arenas and preloaded code; `beam_rt`'s is its much smaller in-process heap).

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

Every case printed the **same** stdout under both runtimes (report note: `add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5 base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, p50 (ms); per-case peak RSS, p50 (MiB, base 2):

| case | sys wall | rust wall | wall ratio | sys RSS | rust RSS | RSS ratio |
|---|---|---|---|---|---|---|
| `add` (fixture) | 133.06 | 148.05 | 1.11× | 66.1 | 18.2 | 3.63× |
| `fib` (fixture, n=20) | 132.93 | 145.80 | 1.10× | 66.2 | 18.2 | 3.64× |
| `pick` (fixture) | 131.41 | 145.96 | 1.11× | 66.4 | 18.3 | 3.64× |
| `calendar_last_day` | 137.63 | 151.23 | 1.10× | 66.3 | 19.1 | 3.47× |
| `queue_from_size` | 132.72 | 155.15 | 1.17× | 66.3 | 22.5 | 2.95× |
| `gb_trees_size` | 131.46 | 151.49 | 1.15× | 66.0 | 18.9 | 3.49× |
| `base64_encode_size` | 134.94 | 150.75 | 1.12× | 66.2 | 20.2 | 3.27× |
| `ordsets_from_size` | 135.05 | 149.58 | 1.11× | 66.7 | 18.5 | 3.62× |
| `string_str_length` | 132.69 | 166.73 | 1.26× | 66.1 | 36.9 | 1.79× |

Whole-workload peak RSS: sys **72 065 024 B (68.7 MiB)** / rust **39 096 320 B (37.3 MiB)** (1.84× lower).

**Read this table as *startup*, not *execution*.** Six of the nine cases call a *system* module, and a `beam_rt` process that must load one first spawns `erl` once to resolve the code path (the same fixed cost that dominates §3.1).
The three pure-fixture cases (`add`/`fib`/`pick`) are also startup-dominated here because each is a single call.
These wall times therefore measure boot-into-work, not interpreter throughput — §3.4 is the per-call comparison.

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source* (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system `erlc` and executed under both runtimes.
`ns/call` is the startup-cancelled marginal `(mean wall@N − mean wall@0) / N`; `N` is per runtime so both marginals clear the timer noise.
`wall` is **not** comparable across runtimes (different `N`); `startup` is.

| case (real function) | N sys / rust | sys ns/call | rust ns/call | ratio | startup p50 sys / rust (ms) | RSS p50 sys / rust (MiB) |
|---|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 1 500 000 / 20 000 | 350.4 | 18 845.9 | **53.8×** | 129.7 / 157.3 | 66.1 / 95.1 |
| `cowboy_bstr:capitalize_token/1` | 1 500 000 / 15 000 | 318.8 | 22 968.6 | **72.0×** | 137.2 / 147.7 | 66.6 / 104.5 |
| `emqx_base62:encode/1` | 3 000 000 / 25 000 | 212.0 | 12 471.3 | **58.8×** | 138.0 / 152.9 | 66.4 / 75.1 |
| `emqx_base62:decode/1` | 3 000 000 / 15 000 | 305.3 | 18 973.6 | **62.2×** | 138.7 / 146.9 | 66.7 / 84.6 |

All four cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout (`sys`: `bstr_to_lower=45000000 bstr_capitalize_token=45000000 emqx_base62_encode=48000000 emqx_base62_decode=33000000`; `rust`: `600000 / 450000 / 400000 / 165000` = `N × per_call`).

**This is the execution-bound result.** Startup is comparable (§3.1) while the *per-call* cost of a real foreign function is ~54×–72× higher under `beam_rt`.
The gap is a **real runtime cost, not a harness artifact**: the startup-cancelled marginal is stable and reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`.
It is a `beam_rt` per-call host-context cost, not interpreter dispatch.
Startup latency and peak RSS are the trustworthy cross-runtime comparisons; the four rust `ns/call` rows are snapshots, so re-run the commands in §2 rather than adjusting these numbers.

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS** on the target projects.
The Rust runtime cannot be scheduled on any of them (§5), so there is no Rust column.
Where a second quiet run exists it is noted; §6 records how far these move with host load.

### 4.1 Compiler throughput (primary full run; two focused runs in brackets)

| workload | metric | value |
|---|---|---|
| `erlc_compiler` | wall | 6 297.23 ms [6 112.52 / 6 083.04 ms] |
| | files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| | **throughput** | **9.53 files/s** [9.82 / 9.86] (**7 384.67 LOC/s**; [7 607.83 / 7 644.69]) |
| | peak RSS | 134 451 200 B (128.2 MiB) |
| `elixir_compiler` | wall | 45 843.60 ms [46 913.42 / 47 655.42 ms] |
| | files attempted / `.beam` produced | 105 / **223** (97/105 compiled) |
| | **throughput** | **4.86 files/s** [4.75 / 4.68] (**2 442.30 LOC/s**; [2 386.61 / 2 349.45]) |
| | compiler startup | 288.67 ms [297.24 / 299.25 ms] |
| | peak RSS | 205 041 664 B (195.5 MiB) |
| | notes | batch compile failed → per-file fallback (checkout ↔ installed-version skew); compiler = installed `elixirc` 1.20.4 |

Both compiler workloads are very stable across runs (≤2 % on files/s).

### 4.2 Cowboy hello-world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s)

Primary full run (host load 4.77 → 11.59); the quieter focused run in brackets:

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 40 261.28 ms [43 205.89 ms] |
| requests | 1 120 867 [923 927] |
| **throughput** | **224 103 req/s** [184 699 req/s] |
| errors | 0 |
| latency min / p50 / p90 | 0.0240 / 0.0613 / 0.0926 ms [0.0297 / 0.0815 / 0.1129 ms] |
| **latency p99** | **0.1750 ms** [0.2176 ms] |
| latency max | 2.418 ms [2.114 ms] (n = 20 000) |
| peak RSS | 173 789 184 B (165.7 MiB) [147 116 032 B (140.3 MiB)] |

### 4.3 Khepri test suite (GC / data-structure bound)

`rebar3` 3.27.1 resolved from the Nix store via a `PATH` prefix (§2). Primary full run; quieter run in brackets:

| metric | value |
|---|---|
| `rebar3 eunit` wall | **76 503.14 ms** [82 155.65 ms] |
| peak RSS | 352 411 648 B (336.1 MiB) [372 711 424 B (355.4 MiB)] |

## 5. Workloads that did **not** run — verified reasons

Every reason below was reproduced on this host, not assumed.

| workload | status | verified reason |
|---|---|---|
| `emqx` | `skipped(not-a-rebar3-project)` | `/home/bill/Source/emqx/rebar.config` **does not exist** — EMQX builds via `make`/`mix`, so `rebar3 compile` would succeed while compiling nothing. The workload detects that (root `rebar.config` missing) and reports the honest skip instead of a meaningless wall time, even with `rebar3` resolvable. |
| `plb` | `skipped(dotnet-absent)` by default; **`failed(plb-tool-failed)`** with the .NET 9 SDK on `PATH` | With `dotnet` on `PATH` (`9.0.318`) PLB's tool builds, but it still yields **no** number: `BenchTool --task bench` requires a prior `--task build` stage, and it throws `System.IO.DirectoryNotFoundException: …/bench/build/elixir_linux_elixir_latest_default_*` (`Program.cs:617`) because that build output does not exist — the build stage needs a docker/`mix local.hex` toolchain with a writable home, and `$HOME` is read-only here. The row is recorded `failed(plb-tool-failed)`; **no PLB number is reported.** |
| all 6 `sys-only` workloads scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/bootstrap an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. |

## 6. Reproducibility, stability and known measurement caveats

**Only the quiet primary run is quoted as the headline.** The three focused `baseline,slice,foreign` runs (`results/m4-bench-xrun-*.json`) were taken under heavy contention (`/proc/loadavg` ~22–37) and their absolute values are inflated.
Across those three runs the rust `foreign` `ns/call` values span **12 282–50 077** and the sys values **234–487** (the sys spread is smaller, but the per-case rust/sys ratio still ranges **~39×–112×**; the quiet primary run gives the tighter **54×–72×**).
The `bench compare` diffs (`results/m4-compare-xrun-run1-vs-run2.txt`, `m4-compare-xrun-run2-vs-run3.txt`, 76 rows each) show 44 and 34 rows moving by more than ±5 % respectively — most of them `_startup_ms` and the startup-dominated `slice` walls.
Treat the xrun set as the **error bar**, not as a second headline.

**Startup across all four runs** (primary + three xruns, p50 ms):

| run | sys p50 | rust p50 | sys p99 | rust p99 |
|---|---|---|---|---|
| primary (quiet) | 117.73 | 148.12 | 137.64 | 169.79 |
| xrun 1 | 128.97 | 157.82 | 180.17 | 200.02 |
| xrun 2 | 125.98 | 166.99 | 239.18 | 220.79 |
| xrun 3 | 122.24 | 386.23 | 142.07 | 589.12 |

The rust startup is the most load-sensitive row (up to ~3.5× under contention); the sys side stays within ~1.1–1.7×.

Caveats an honest reading requires:

1. **These numbers are snapshots, not constants.** The host is shared and `erl` (a 14-scheduler SMP VM) loses more to CPU contention than `beam_rt` does. Prefer ratios, prefer the quiet primary run, and record `/proc/loadavg` alongside any absolute figure.
2. **`beam_rt` resolves its system code path by spawning `erl` once per process**, so it affects every process that touches a system module — all `slice` wrapper cases, all `foreign` cases, and the `baseline` startup — and is the reason Rust startup is ~148 ms rather than a few ms. Setting `$ERTS_STDLIB_DIR` skips the spawn; `bench` never sets it (the harness is a pure driver/observer and never mutates the runtime's environment beyond argv). The per-call metric cancels this cost by construction.
3. **`elixir_compiler` measures the installed `elixirc` 1.20.4** against the unbuilt, read-only 1.21.0-dev checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the artifact.
4. **`cowboy` re-fetches cowlib/ranch** in a fresh scratch copy on every run (network), which is why its `build_ms` (~40–43 s) dominates the workload.
5. Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`); Erlang's own `erlang:memory(total)` is a **separate** metric (§3.2) and is never conflated with it.
6. **`foreign` `wall_ms` is not comparable across runtimes** — each side runs a different `N`. Compare `ns/call` and `startup_ms`.
7. **`foreign` peak RSS is not a like-for-like memory comparison.** `beam_rt` does not yet drive a garbage collector, so a looping program accumulates garbage in proportion to its loop count `N`, and the rust `foreign` rows run at large `N` — giving a rust per-case peak RSS of **78 708 736–109 623 296 B (75.1–104.5 MiB)** that is *higher* than the sys figures on the same cases (69 287 936–69 953 536 B). Same-`N` memory is unchanged; the row moves with the calibration. Use `slice` (§3.3, one call per process) as the comparable per-process memory metric: rust 18.2–36.9 MiB vs sys 66.0–66.7 MiB.

## Archived artifacts

| file | contents |
|---|---|
| `results/m4-bench-full-runtimes-all.json` | **primary full run**: `--full --runtime all`, all 9 workloads × both runtimes — the source of §3 and the headline §4 numbers |
| `results/m4-bench-xrun-baseline-slice-foreign.json` | focused cross-runtime run 1: `--full --runtime all --workloads baseline,slice,foreign` |
| `results/m4-bench-xrun-baseline-slice-foreign-run2.json` | run 2 of the focused set |
| `results/m4-bench-xrun-baseline-slice-foreign-run3.json` | run 3 of the focused set |
| `results/m4-bench-full-compilers-sys.json` | `erlc_compiler` + `elixir_compiler`, `--full --runtime sys` |
| `results/m4-bench-full-compilers-sys-run2.json` | the compiler repetition (§4.1 brackets) |
| `results/m4-bench-full-cowboy-sys.json` | `cowboy`, `--full --runtime sys`, quiet window (§4.2 brackets) |
| `results/m4-bench-full-khepri-emqx.json` | `khepri` + `emqx`, `--full --runtime sys` with `rebar3` on `PATH` (§4.3 brackets, `emqx` skip) |
| `results/m4-bench-quick-plb-net9-attempt.json` | the PLB attempt with the .NET 9 SDK on `PATH` → `failed(plb-tool-failed)` |
| `results/m4-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside the primary full run |
| `results/m4-compare-xrun-run1-vs-run2.txt` | `bench compare` — focused run 1 vs run 2 (76 rows) |
| `results/m4-compare-xrun-run2-vs-run3.txt` | `bench compare` — focused run 2 vs run 3 (76 rows) |

The archived copies are byte-for-byte the harness output except for one mechanical normalisation: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt` so the artifact carries no environment-specific absolute path.
**No metric value is altered.**
