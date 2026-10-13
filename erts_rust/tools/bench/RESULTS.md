# Rust-ERTS benchmark results — current measurement run (m10)

Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured, never modelled**.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-13 23:06–23:15 UTC** (= 2026-10-14 07:06–07:15 +0800) at commit **`b4593818b3a27f47e3b682a1a50fbfcc0a1785fe`** (`b4593818b3`, the M10 milestone head), with the driver and `beam_rt` both built from this tree (`--release -p beam_rt -p bench`).
Raw harness JSON is committed under `results/` as `m10-*`; the previous runs are kept as `m9-*` (2026-10-13), `m6-*` (2026-10-11), `m5-*` (2026-10-09) and `m4-*` (2026-10-08) and are the regression baselines for the delta section (§7).

> **Naming both sides of every ratio (read this before quoting a number).** `sys` = the **system ERTS** (OTP 29.1.1, the oracle `erl`, a 14-scheduler SMP VM). `rust` = **`beam_rt`**, the Rust runtime, at its **default execution tier** (the native baseline JIT). A third, distinct column exists — **`rust` with `BEAM_RT_TIER=interpreter`** — and it is a **Rust-vs-Rust tier effect, NOT a system-ERTS comparison** (§3.9, §8). The m9 record once published a `foreign` figure of **0.71×–0.84×** as "of the system ERTS" when it was actually that Rust-vs-Rust tier effect; against the system ERTS the same loop was **46.2×–74.2×** dearer. **Every ratio in this document names both sides.**
> **The target applications are NOT run by the Rust runtime.** `cowboy`, `khepri`, `plb`, `erlc_compiler`, `elixir_compiler` and `emqx` are executed by their **own system-ERTS / system-toolchain** (`erlc`, `elixirc`, the Cowboy release on the oracle `erl`, `rebar3`'s Erlang, the `.NET` tool). They are **reference** figures for the sys-only surfaces (§4), never Rust-runtime results.

> **This is a shared, usually-loaded host** (§1, §6.1): absolute latencies/throughputs move several-fold with load, so **ratios are the signal** and a single contended sample is never a constant. The m10 passes ran at 1-min loads `5.43 → 7.59` (cross-runtime), `7.94 → 8.03` (tier control) and `21.12 → 10.30` (external targets).

## Verdict

**(a) / (b) Which targets run, which skip.** Five of the six external target projects run; one (`emqx`) skips with a verified toolchain reason (never a fabricated number). All five running rows are executed by their **own system toolchain**, not by the Rust runtime.

| target project | runs? | executed by | result / verified reason |
|---|---|---|---|
| `khepri` (tree DB) | **runs** | `rebar3`'s Erlang (OTP 28 escript; §4.2) | `ok` — `rebar3 eunit` wall 78 762.66 ms |
| `cowboy` (HTTP server) | **runs** | the oracle `erl` release | `ok` — 210 250.49 req/s, p50 0.0636 ms / p99 0.3020 ms |
| `erlc_compiler` (Erlang compiler) | **runs** | system `erlc` (OTP 29) | `ok` — 8.842 files/s over the stdlib+Elixir corpus |
| `elixir_compiler` (Elixir compiler) | **runs** | installed `elixirc` 1.20.4 | `ok` — 4.665 files/s over 105 sources / 223 `.beam` |
| `plb` (Benchmarks Game kernels, .NET) | **runs** | the `.NET` tool (`dotnet` 9.0.318) | `ok` — wall 70 841.43 ms; `test_time_ms` p50 554.76 / p99 3 716.01 (n=10) |
| `emqx` (MQTT broker) | **skips** | — | `skipped(emqx-requires-otp-28)` — EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`), the host `erl` is OTP 29.1.1 / erts-17.1; not satisfiable without a full OTP-28 toolchain swap (§4.3, §5) |

The six workloads the Rust runtime itself can host (`Runtimes::Both`) — `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound` — all ran: **12/12 rows `ok`** (0 skipped, 0 unsupported, 0 failed). Every other workload is `SysOnly` and would report `skipped(runtime-unsupported)` under `rust` (§5).

**(c) Ratio to the system-ERTS oracle, per cross-runtime workload.** `rust/sys` is the **Rust default tier** cost as a multiple of the **system-ERTS** cost (`<1` ⇒ Rust cheaper, `>1` ⇒ Rust dearer). Values are the m10 `xrun` (§3, `--full`, 1-min load 5.43 → 7.59).

| workload | surface | headline metric | sys (system ERTS) | rust (`beam_rt`, default tier) | rust/sys | reading |
|---|---|---|---|---|---|---|
| `baseline` | startup / CLI | `startup_ms` (mean, whole process) | 114.3603 ms | 15.5243 ms | **0.136×** | Rust **7.37× faster** per process |
| `slice` (9 programs) | compute | `case_*_wall_ms` (mean, per case) | 135.08–141.48 ms | 15.38–48.12 ms | **0.109×–0.352×** | Rust **2.84×–9.20× faster** |
| `foreign` (Cowboy/EMQX source) | compute / `.beam` | `case_*_ns_per_call` | 210.88–339.42 ns | 16 326.00–22 566.61 ns | **52.4×–78.7×** | Rust **52.4×–78.7× dearer** per call |
| `list_walk` | dispatch | `case_list_walk_ns_per_element` | 2.1431 ns | 47.3880 ns | **22.1×** | Rust **22.1× dearer** per element |
| `copy_bound` (ETF / ETS / send-recv) | Msg / copy | `case_*_ns_per_op` | 240.47–1 453.98 ns | 3 267.85–79 518.12 ns | **13.6×–54.7×** | Rust **13.6×–54.7× dearer** per op |
| `gc_bound` (allocation churn) | GC | `case_churn_ns_per_op` | 4 364.91 ns | 147 194.37 ns | **33.7×** | Rust **33.7× dearer** per op |

- **The two directions are both real and answer different questions.** Rust's *whole-process cost* is far lower (a `beam_rt` process starts in ~15.5 ms vs the oracle's ~114 ms), while the *per-unit execution cost* of Erlang code under `beam_rt` is far higher (**13.6×–78.7×**, §3.8). Startup / whole-process cost is the trustworthy whole-process comparison; the startup-cancelled marginals are the per-unit execution-throughput signal.
- **The per-unit band narrowed a lot since m9** — the `list_walk` per-element gap fell **65.5× → 22.1×**, `etf_roundtrip` **118.5× → 15.1×**, `send_recv` **114.4× → 54.7×**, `gc_bound churn` **42.2× → 33.7×**; `foreign` stayed in band (**46.2×–74.2× → 52.4×–78.7×**, §7).
- **Peak RSS is a like-for-like cross-runtime comparison only for `slice`** (one call per process). For `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS still grows with the per-runtime loop count `N` and is **not** comparable (§6.7) — though `list_walk` and `gc_bound` now sit **below** the oracle (§7.4).
- **Load caveat (read before quoting any absolute number).** The m10 passes ran at 1-min `/proc/loadavg` `5.43 → 7.59` (cross-runtime), `7.94 → 8.03` (tier control) and `21.12 → 10.30` (external targets); the host is shared and absolute latencies are **not comparable across runs**. Prefer the ratios above, the startup-cancelled marginals and the deterministic counters.

## Coverage — workload ↔ rewrite surface

The driver exercises the messaging/copy, ETF, ETS and GC surfaces named in the top-level `ERTS_RUST_REWRITE_TEST_PROJECTS.md` coverage matrix, **under both runtimes**, via the workloads `copy_bound` / `gc_bound`; the I/O, compute and compiler surfaces are covered too (the I/O and compiler ones only on the system runtime).

| workload | category | rewrite surface exercised (TEST_PROJECTS matrix) | runtime |
|---|---|---|---|
| `baseline` | startup | startup / p99 tail; CLI (A.6); boot (A.7) | **both** |
| `slice` | compute | JIT/compute (calls real `calendar`/`queue`/`gb_trees`/`base64`/`ordsets`/`string` code) | **both** |
| `foreign` | compute | JIT/compute on **real Cowboy + EMQX source**, compiled by `erlc` (`.beam` A.2) | **both** |
| `list_walk` | compute | JIT/compute — a BIF-free body-recursive walk (per-element dispatch) | **both** |
| `copy_bound` | messaging | **Msg/copy + Distrib/ETF + ETS** — `term_to_binary`/`binary_to_term`, `ets:insert`+`ets:lookup`, two-process send/receive | **both** |
| `gc_bound` | gc | **GC** — allocate-and-abandon churn | **both** |
| `cowboy` | io | I/O — Ranch/`epoll` socket path, `cowlib` HTTP parse, per-request process spawn | system-only |
| `khepri` | gc | ETS/`ordered_set`, GC pressure, disk I/O | system-only |
| `plb` | compute | JIT/compute — Benchmarks-Game kernels (Erlang/Elixir) | system-only |
| `erlc_compiler` | compiler | compiler throughput (`erl_parse`/`erl_lint`/`beam_ssa_*`), file I/O | system-only |
| `elixir_compiler` | compiler | compiler throughput (Elixir toolchain) | system-only |
| `emqx` | messaging | Msg/copy, Distrib/ETF, NIF ABI (`ssl`/`crypto`) | system-only (**skipped**, §4.3) |

**Surfaces still uncovered by this driver:** the **NIF C ABI** (A.3) and **driver C ABI** (A.4), the **Hot-upgrade** surface, and the **live distribution wire** — `copy_bound`'s `etf_roundtrip` case covers the **ETF codec** (A.5) as a single-process round trip, not a two-node handshake / `DOP_*` loopback. Those surfaces are covered by the differential gate (`tools/differential`, contracts A.3/A.4), not by `bench`.

## Host contention (read before quoting any absolute number)

> The benchmark host is a **shared 14-core machine** and was oversubscribed during m10 at times.
> The primary (**full**) passes were taken at `/proc/loadavg` 1-min **5.43 → 7.59** (cross-runtime), **7.94 → 8.03** (tier control) and **21.12 → 10.30** (external targets); the PLB pass ended at **27.43** (`results/m10-load-record.md` records the exact sequence).
> Every *absolute* latency / wall / throughput below is still inflated by CPU contention and is **not comparable across runs**. Prefer **ratios**, the **startup-cancelled per-op / per-call / per-element marginals** (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counters** (`*_ops`, `*_elements`).
> `/proc/loadavg` is host-wide and a weak proxy — it does **not** move monotonically with the measured figures (§7) — so record it, but never infer a slowdown from it alone.
> The load was recorded **before and after every run** (§1/`results/m10-load-record.md`) — mandatory here.

## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (UTC) | 2026-10-13 23:06–23:15 UTC (= 2026-10-14 07:06–07:15 +0800) |
| Commit measured | `b4593818b3a27f47e3b682a1a50fbfcc0a1785fe` |
| Host / kernel | Linux 7.2.9 (`#1-NixOS SMP PREEMPT_DYNAMIC`), `x86_64` |
| Distribution | NixOS 26.11 (Zokor) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 65 317 432 kB (62.29 GiB) |
| Host load (1-min `/proc/loadavg`) | full cross-runtime `xrun` **5.43 → 7.59**; tier control **7.94 → 8.03**; full external targets **21.12 → 10.30**; PLB **10.04 → 27.43**, EMQX **27.43** (per-run table in `results/m10-load-record.md`) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl` at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`, **OTP 29.1.1 / erts-17.1** (`otp_release` = `"29"`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, resolved from the Nix store by the driver's **store scan** (**not** on `PATH`); its shebang runs it under **Erlang/OTP 28** — see §4.2 |
| `dotnet` | **9.0.318, on `PATH`** (`/run/current-system/sw/bin/dotnet`). The store also holds two 8.0.425 SDKs; the version-aware resolver rejects them for `plb` (declared min `9`) and records each rejection (§4.1) |
| `clang` | **21.1.8**, on `PATH` |
| `$BENCH_TRY_NIX` | not needed and not set for the m10 runs — the store scan resolves `rebar3` locally (§4.2) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` |
| `/usr/bin/time` | **absent** — the driver's own timers and `/proc/<pid>` `VmHWM` polling are used |

Both runtimes resolve and pass the harness self-check (`supported: true`).

Per-run `/proc/loadavg` (1-min value first; the full sequence, with the exact commands, is in `results/m10-load-record.md`):

| run | before | after |
|---|---|---|
| **FULL cross-runtime pass** (`m10-bench-xrun.json`, wall 2m36s) | `5.43` | `7.59` |
| **FULL tier control** (`m10-bench-xrun-interp.json`, rust-only, wall 41s) | `7.94` | `8.03` |
| focused `copy_bound` / `foreign` quick re-checks | `10.18`–`10.81` | `10.74` |
| **FULL external-target pass** (`m10-bench-external.json`, wall 3m00s) | `21.12` | `10.30` |
| PLB quick pass (`m10-bench-plb.json`, wall 1m34s) | `10.04` | `27.43` |
| EMQX quick pass (`m10-bench-emqx.json`, skips immediately) | `27.43` | `27.43` |

## 2. Exact commands

Build the driver and the runtime:
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
```
Probe status:
```sh
erts_rust/target/release/bench list --json
```
The **focused cross-runtime run** — all six `Runtimes::Both` workloads, `--full`, both runtimes → `results/m10-bench-xrun.json` (wall 2m36s):
```sh
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m10-bench-xrun.json --verbose
```
The **tier-effect control** — the same workloads, `rust` only, interpreter pinned → `results/m10-bench-xrun-interp.json` (wall 41s):
```sh
BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --full --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m10-bench-xrun-interp.json
```
Cross-runtime diff and tier diff (archived as text artifacts; the tool diffs the **mean**):
```sh
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m10-bench-xrun.json \
    > erts_rust/tools/bench/results/m10-compare-cross-runtime.txt
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m10-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m10-bench-xrun.json \
    > erts_rust/tools/bench/results/m10-compare-tier-effect.txt
```
The **full sys-only external-target run** (one report, four workloads; wall 3m00s) and the two remaining single-target runs:
```sh
erts_rust/target/release/bench run --full  --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/out/m10-bench-external.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/out/m10-bench-plb.json  --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/out/m10-bench-emqx.json --verbose
```
Crate tests: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` (**250 passed / 0 failed** at this commit).

Profile semantics: `--full` ⇒ `baseline` 50 startup samples / 5 compute samples, `slice` 25 timed runs/case, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200 files.
`--quick` ⇒ `baseline` 10 / 5, `slice` 5, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 3, `cowboy` 8 connections × 2 s.
The per-runtime loop count `N` in the `foreign`/`list_walk`/`copy_bound`/`gc_bound` cases is **fixed per case** (not profile-dependent), so the `--full` sample counts only widen the estimate, never the loop length (§3.4–3.7).
**Run order is runtime-major**: all `sys` rows execute first, then all `rust` rows — under a *drifting* host load the two sides are measured in different windows (§6.1).

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

`sys` = **system ERTS**, OTP 29.1.1 with the JIT. `rust` = **`beam_rt`**, release, at its **default execution tier**: a working runtime whose per-process tier **defaults to the native baseline JIT** (`JitMode::Auto` ⇒ `Tier::BaselineJit` on the process's first entry, remembered across pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the only kill switch), with anything outside the modelled `genop.tab` surface returning a typed `unsupported`/`BeamError` rather than a wrong result. Its runtime path compiles **per function, lazily** on a function's first entry (`../../crates/beam_engine/src/jit/baseline/code.rs`) — never whole-module; the tier effect and the one-shot cold start are in §3.9.
All tables in this section come from the **focused `xrun`** (`results/m10-bench-xrun.json`, `--full --runtime all`, 1-min load 5.43 → 7.59), which runs the six cross-runtime workloads back to back: **12/12 rows `ok`**.

### 3.1 Startup latency — `baseline` (n = 50, ms)

| stat | sys (system ERTS) | rust (`beam_rt`, default tier) | rust/sys |
|---|---|---|---|
| min | 93.49 | 12.44 | 0.133× |
| **mean** | **114.36** | **15.52** | **0.136× (rust 7.37× faster)** |
| p50 | 112.90 | 15.65 | 0.139× |
| p90 | 121.64 | 16.15 | 0.133× |
| **p99** | **145.16** | **16.52** | **0.114× (rust 8.79× faster)** |
| max | 145.52 | 16.52 | 0.114× |

Rust startup is both lower **and far flatter** (12.4–16.5 ms across 50 samples). This is not bare interpreter boot against a like start: the oracle is a 14-scheduler SMP VM whose boot loses much more to the contended/oversubscribed host, while a `beam_rt` process is a small single-threaded binary that resolves its code path **natively from the filesystem**, with no `erl` spawn (§6.2). The whole-process figures are therefore the trustworthy cross-runtime startup comparison.

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)

| metric | sys (system ERTS) | rust (`beam_rt`) | rust/sys |
|---|---|---|---|
| `compute_loop_ms` p50 | 188.65 | 62.29 | 0.330× |
| `compute_ops_per_sec` | 5 243 415 | 16 418 845 | 3.13× (rust faster) |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 36 229 856 B (34.56 MiB) | **805 458 B (786.6 KiB)** | **44.98× lower** |
| peak RSS (`VmHWM`, whole workload) | 136 417 280 B (130.1 MiB) | 31 612 928 B (30.1 MiB) | 4.32× lower |

The loop is `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'` and runs under **both** runtimes.
**Caveat — read this loop as the native floor, not interpreter throughput.** It is dominated by the natively-served `lists:seq/2` + `lists:sum/1`; the execution-bound signal is §3.4–3.7.
`vm_memory_total_bytes` is a **separate** metric from the OS `peak_memory_bytes` (§6.5) and is never conflated with it: `beam_rt` serves `erlang:memory/0,1` through the `MemoryHook` seam, and the two runtimes are not reporting the same quantity (OTP's `total` includes allocator arenas and preloaded code; `beam_rt`'s is its much smaller in-process heap).

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

Every case printed the **same** stdout under both runtimes (the report's `observed stdout:` note: `add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5 base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, **mean** (ms); per-case peak RSS, **p50** (MiB, base 2):

| case | sys (system ERTS) wall | rust (`beam_rt`) wall | rust/sys wall | sys RSS | rust RSS | sys/rust RSS |
|---|---|---|---|---|---|---|
| `add` (fixture) | 138.61 | 15.38 | 0.111× | 66.33 | 14.61 | 4.54× |
| `fib` (fixture, n=20) | 135.88 | 16.22 | 0.119× | 66.71 | 15.21 | 4.39× |
| `pick` (fixture) | 135.08 | 16.10 | **0.119×** | 66.61 | 15.18 | 4.39× |
| `calendar_last_day` | 136.06 | 18.66 | 0.137× | 66.25 | 15.93 | 4.16× |
| `queue_from_size` | 140.36 | 26.00 | 0.185× | 66.54 | 17.95 | 3.71× |
| `gb_trees_size` | 135.92 | 25.12 | 0.185× | 66.29 | 16.00 | 4.14× |
| `base64_encode_size` | 141.48 | 26.68 | 0.189× | 66.50 | 17.15 | 3.88× |
| `ordsets_from_size` | 135.33 | 26.15 | 0.193× | 66.31 | 15.78 | 4.20× |
| `string_str_length` | 136.51 | 48.12 | **0.352×** | 66.29 | 26.32 | 2.52× |

Whole-workload peak RSS: sys **71 868 416 B (68.5 MiB)** / rust **27 848 704 B (26.6 MiB)** (2.58× lower).

**Read the wall column as *whole-process*, not *execution*.** Each case is a single call per process, so the wall time is dominated by process boot; six of the nine also call a *system* module. Per-case `rust` peak RSS **is** the like-for-like per-process memory comparison (one call per process, no loop accumulation): rust **14.6–26.3 MiB** vs sys **66.3–66.7 MiB** (2.52×–4.54× lower).
The `string_str_length` row is the family's **one-shot cold-start** case — the first entry pulls in a large Erlang module graph — and it is the one row well above the ~15–16 ms boot floor (§3.9).

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source* (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system `erlc` and executed under both runtimes.
`ns/call` is the startup-cancelled marginal `(mean wall@N − mean wall@0) / N` (n = 10 samples each); `N` is per runtime so both marginals clear the timer noise. `wall_ms` is **not** comparable across runtimes (different `N`); `startup_ms` is.

| case (real function) | N sys / rust | sys ns/call | rust ns/call | rust/sys | startup p50 sys / rust (ms) | RSS p50 sys / rust (MiB) |
|---|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 1 500 000 / 20 000 | 339.42 | 17 789.39 | **52.41×** | 132.73 / 15.24 | 66.40 / 73.76 |
| `cowboy_bstr:capitalize_token/1` | 1 500 000 / 15 000 | 311.83 | 22 566.61 | **72.36×** | 134.55 / 16.57 | 66.46 / 76.40 |
| `emqx_base62:encode/1` | 3 000 000 / 25 000 | 210.88 | 16 601.05 | **78.72×** | 132.18 / 16.24 | 66.62 / 61.84 |
| `emqx_base62:decode/1` | 3 000 000 / 15 000 | 293.93 | 16 325.99 | **55.55×** | 120.27 / 16.55 | 66.28 / 62.95 |

All four cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout (`sys`: `bstr_to_lower=45000000 bstr_capitalize_token=45000000 emqx_base62_encode=48000000 emqx_base62_decode=33000000`; `rust`: `600000 / 450000 / 400000 / 165000` = `N × per_call`).

**This is the execution-bound result.** Startup is comparable while the *per-call* cost of a real foreign function is **~52×–79×** higher under `beam_rt`. The gap is a **real runtime cost, not a harness artifact**: the startup-cancelled marginal is stable and reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`. It is a `beam_rt` per-call host-context cost, not the engine's per-instruction dispatch (§3.8).
**`foreign` peak RSS is not a like-for-like memory comparison — see §6.7.**
**One case moved the wrong way vs m9:** `emqx_base62:encode/1` **10 283 → 16 601 ns/call (+61 %)** while the other three improved 8–9 % (§7.2, §8 item 4).

### 3.5 `list_walk` — the per-element dispatch-throughput metric (n = 10/case)

A **BIF-free** body-recursive walk (`walk(N)` sums a 1000-element list `N` times; `build/2` makes the list once per process) runs under both runtimes; the reported figure is the startup-cancelled **ns per walked element**.

| runtime | `N` (walks) | elements walked | `ns_per_element` | `wall_ms` mean | `startup_ms` mean | peak RSS p50 |
|---|---|---|---|---|---|---|
| `sys` (system ERTS) | 100 000 | 100 000 000 | **2.1431** | 354.84 | 140.53 | 66.43 MiB |
| `rust` (`beam_rt`) | 1 000 | 1 000 000 | **47.3880** | 64.18 | 16.79 | 15.32 MiB |

`case_list_walk_elements` (the deterministic, contention-immune companion metric) is `1×10⁸` for sys and `1×10⁶` for rust; each run printed the deterministic constant `N × 500 500` (`rust`: `list_walk=N*500500=500500000`, `sys`: `…=50050000000`).
`ns/element` gives a **22.11×** rust/sys ratio — the narrowest it has ever been (**65.5×** in m9, **108.7×** in m6): the m10 baseline-JIT Y-slot arithmetic + continuation sub-blocks cut the Rust per-element cost **158.83 → 47.39 ns** (§7.2).

### 3.6 `copy_bound` — the copy boundary: ETF, ETS and process send/receive (n = 10/case)

Three copy-boundary cases under both runtimes, each validating its own round-trip with a bound-variable pattern: an **ETF** `term_to_binary`/`binary_to_term` round trip, an **ETS** `ets:insert` + `ets:lookup` round trip, and a **two-process send/receive** copy. All three are startup-cancelled `ns/op`; the `ops` counter is deterministic.

| case | op | sys ns/op | rust ns/op | rust/sys | ops sys / rust | rust RSS p50 |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | `term_to_binary` / `binary_to_term` | 651.9348 | 9 838.00 | **15.09×** | 1 500 000 / 5 000 | 19.25 MiB |
| `ets_copy` | `ets:insert` + `ets:lookup` | 240.4679 | 3 267.85 | **13.59×** | 2 000 000 / 100 000 | 90.30 MiB |
| `send_recv` | two-process send/receive copy | 1 453.9765 | 79 518.12 | **54.69×** | 500 000 / 2 000 | 20.99 MiB |

All three ran under both runtimes (`3 ok, 0 unsupported`) with identical stdout (`sys`: `etf_roundtrip=1500000 ets_copy=2000000 send_recv=500000`; `rust`: `5000 / 100000 / 2000` = `N × 1`).
`ets_copy` is the cheapest boundary for both runtimes, and its rust/sys ratio (13.6×) is the narrowest in the whole suite; `etf_roundtrip` fell **118.5× → 15.1×** vs m9 (the ETF atom-table-clone removal, §7.2) and is now in the same band as `ets_copy`; `send_recv` remains the dearest copy path at **54.7×**.
**`copy_bound` rust peak RSS still grows with `N` — the `ets_copy` row (N = 100 000) reaches 90.30 MiB vs sys 66.26 MiB (§6.7, §8 item 5).**

### 3.7 `gc_bound` — allocate-and-abandon churn (n = 10/case)

`churn(N)` allocates and immediately abandons a 256-element list, a 256-byte binary and a 256-element tuple, `N` times; the reported figure is the startup-cancelled `ns/op`.

| runtime | `N` (churns) | `churn_ns_per_op` | `wall_ms` mean | `startup_ms` mean | peak RSS p50 |
|---|---|---|---|---|---|
| `sys` (system ERTS) | 200 000 | **4 364.9104** | 998.90 | 125.92 | 66.77 MiB |
| `rust` (`beam_rt`) | 2 000 | **147 194.3719** | 310.70 | 16.31 | **15.40 MiB** |

`case_churn_ops` (deterministic) is `200 000` (sys) / `2 000` (rust); each run printed `churn=N*1` (matching its `N`).
The rust/sys ratio is **33.72×**. This row measures the allocate-and-abandon **wall time**; whether a runtime's collector actually runs during the loop is its own admission policy and is **not claimed here** (the report's own note says the same).
**The rust peak RSS is the figure that moved most since m9 — 38.78 → 15.40 MiB (p50), and now *below* the oracle's 66.77 MiB**: the BIF-result copy-out + pooled-heap rewind bounds this loop's memory (§7.2).

### 3.8 Reading the cross-runtime numbers

- **Startup / whole-process cost is the trustworthy cross-runtime comparison** (§3.1, §3.3): rust is 7.37× faster at `baseline` startup (mean) and 2.84×–9.20× faster per `slice` case, with 2.58× lower per-process peak RSS. A `beam_rt` process starts in ~15.5 ms.
- **The startup-cancelled marginals are the per-unit execution-throughput signal** (§3.4–3.7), and they point the other way: rust costs **13.6×–78.7×** more per op/call/element. That residual is a `beam_rt` per-call runtime cost, not the engine's per-instruction dispatch (next bullet).
- **Only `slice` gives a like-for-like peak-RSS comparison** (§6.7). `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS still grows with the per-runtime loop count `N`.
- The gap is a **`beam_rt` per-call host-context cost**, not `beam_engine` instruction dispatch: the engine's own side of a BIF call is `~11–12 ns` (`../../crates/beam_engine/tests/bif_cost.rs`), i.e. **≲0.15 %** of the runtime's per-call figure, so **~99.85 % is HOST-side** (`beam_rt`'s `ExecutionContext::call_bif`, `../../crates/beam_rt/src/context/calls.rs`). The runtime-side figures are owned by `../../crates/beam_rt/PERF.md` and `../../crates/beam_rt/CONTEXT.md`, not `bench` output.
- **The `slice` whole-process column includes a one-shot cold start, and that cold start is bounded** (§3.9): the runtime path lowers a function on its first entry, so a process that enters a large Erlang module graph pays the first-entry compile cost rather than a whole-module compile.

### 3.9 One-shot cold start, and the **tier effect** (what the JIT default does and does not improve)

Every `slice` case is a single call in a **fresh** `beam_rt` OS process (§3.3), so that wall column carries the cost of entering a system module for the first time. That cost is bounded by construction: the **runtime path** stages a **lazy** native artifact — one sealed mapping per function, lowered on that function's first entry of a `(func, pc)` (`../../crates/beam_engine/src/jit/baseline/code.rs`) — so a fresh process pays only for the functions it actually enters. The **eager** whole-module layout (`JitBackend::compile` / `CodeCache::get_or_compile`) exists for the whole-module introspection surface and for the `jit_bench` harness; it is **not** the runtime path. The module-graph-heavy `string_str_length` row is the family's visible cold-start case (rust 48.12 ms mean vs the ~15–16 ms boot floor, §3.3).

**The tier effect is a Rust-vs-Rust comparison — it is NOT a comparison against the system ERTS.** The default `beam_rt` tier is the native baseline JIT; `BEAM_RT_TIER=interpreter` forces the interpreter. The delta between those two *Rust* columns is the **tier effect** (the JIT making the *Rust* runtime's own interpreter work cheaper, or occasionally dearer), measured this pass as a full `--full` `rust`-only run (`results/m10-bench-xrun-interp.json`) and archived as `results/m10-compare-tier-effect.txt`:

| workload | metric | rust default (baseline JIT) | rust `BEAM_RT_TIER=interpreter` | interp/default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 15.5243 | 15.8192 | 1.019× |
| `baseline` | `compute_loop_ms` | 60.9056 | 59.0437 | 0.969× |
| `foreign` | `bstr_to_lower` ns/call | 17 789.39 | 21 959.53 | 1.234× |
| `foreign` | `bstr_capitalize_token` ns/call | 22 566.61 | 27 023.90 | 1.198× |
| `foreign` | `emqx_base62_encode` ns/call | 16 601.05 | 20 292.12 | 1.222× |
| `foreign` | `emqx_base62_decode` ns/call | 16 325.99 | 23 850.45 | 1.460× |
| `list_walk` | `ns_per_element` | 47.3880 | 181.9423 | **3.839×** |
| `copy_bound` | `etf_roundtrip` ns/op | 9 838.00 | 10 676.66 | 1.085× |
| `copy_bound` | `ets_copy` ns/op | 3 267.85 | 4 165.65 | 1.275× |
| `copy_bound` | `send_recv` ns/op | 79 518.12 | 36 969.39 | **0.465×** (default 2.15× *dearer*) |
| `gc_bound` | `churn` ns/op | 147 194.37 | 291 856.40 | 1.983× |

**Reading:** the baseline JIT is a clear win for `list_walk` (3.84×), `gc_bound churn` (1.98×) and the `foreign` per-call rows (1.20×–1.46×), neutral for `startup`/`compute`/`etf`, and **inverted** for the two-process `send_recv` loop — the default tier is **2.15× dearer** than the interpreter, reproduced more strongly (3.31×) by a focused `--quick` re-run (§8 item 2). This is Rust-vs-Rust and says nothing about the system-ERTS gap.

> **Correction carried forward (do not re-introduce the misreading).** The m9 record's `foreign` **0.71×–0.84×** and `gc_bound/churn` **0.49×** were **this tier effect** (Rust default ÷ Rust `BEAM_RT_TIER=interpreter`), **not** a Rust-vs-system-ERTS comparison. Against the *system* ERTS the default-tier rust figures were still far **dearer** (`foreign` **46.2×–74.2×**, `churn` **42.2×** in m9; now **52.4×–78.7×** and **33.7×** in m10, §3.4/§3.7). `CHANGE.md`'s M10 entry records the same correction.

**Caveat — `jit_bench` pins the hot path, not this path.** The engine's `jit_bench` micro-benchmark drives the **eager** whole-module artifact only, so its interpreter → baseline-JIT speed-ups (`../../crates/beam_engine/MEASURED.md`) describe the hot path with the compile amortised away, and say nothing about the lazy runtime path's per-entry compile cost.
The cited documents are **current-state** records — re-run their own commands for the live figures rather than quoting this summary. And the host-load caveat applies to them exactly as it does here (§6.1).

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS / system toolchain** on the target projects.
The Rust runtime cannot be scheduled on any of them (`Runtimes::SysOnly`, §5), so there is no Rust column — and **none of these numbers is a Rust-runtime result**; they are **reference** figures for the sys-only surfaces.
All absolute values in this section are inflated by the varied host load (§1) and are **not comparable across runs**; §7 states each move explicitly.

### 4.1 `plb` — Programming-Language-Benchmarks (dotnet)

| metric | value |
|---|---|
| status | **`ok`** (the version-aware tool resolution picks `dotnet` 9.0.318) |
| `wall_ms` (two-stage `--task build` + `--task bench`) | 70 841.43 |
| build stage | 21 795 ms |
| `test_time_ms` (n = 10, PLB's own per-test time) | min 0 / mean 1 258.29 / p50 554.76 / p90 3 544.79 / p99 3 716.01 |
| `test_mem_bytes` | 92 553 216 B |
| peak RSS (external `VmHWM` of the `.NET` tool) | 170 311 680 B |
| provenance notes | `resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)`; `rejected dotnet 8.0.425 … (nix-store): requires >= 9` ×2 |

The version-aware resolver (`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` with `MIN_DOTNET_SDK = &[9]`) selects the **greatest** satisfying `dotnet` across `PATH` + `$BENCH_TOOL_PATH` + the store and records every rejection, so a too-old `dotnet` on `PATH` can no longer shadow a newer SDK. The m9 post-fix smoke already proved the defect condition (an 8.0.425 SDK forced first on `PATH`) still resolves 9.0.318; m10's `PATH` resolves ≥ 9 directly.
Read these as **`--quick` observations on a shared host**, not milestone figures: `wall_ms` is dominated by building the kernels' escripts and `test_time_ms` is PLB's own per-kernel time (`min` 0 = a sub-millisecond kernel). They say "PLB builds and runs on this host".

### 4.2 `khepri` — Khepri `rebar3 eunit` (GC / data-structure bound)

`rebar3` 3.27.1 is resolved from the Nix store by the driver's **store scan** — no `PATH` prefix and no env-var opt-in are needed (`rebar3` is not on `PATH`). It runs under **Erlang/OTP 28** (its escript shebang), while the report's `runtimes[]` entry describes the driver's oracle.

| metric | value |
|---|---|
| `rebar3 eunit` wall | 78 762.66 ms |
| peak RSS | 363 016 192 B (346.2 MiB) |

### 4.3 `emqx` — EMQX umbrella build (`make emqx-enterprise-compile`)

`emqx` **does not run**: `skipped(emqx-requires-otp-28)`, with no number fabricated. The blocker is a genuine, mechanical toolchain constraint — **the checkout's exactly-declared pin vs the host's actual runtime**, verified first-hand:

* **The exact pin (required).** `28.5.0.7-2` — `/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`, and `/home/bill/Source/emqx/env.sh:3` → `export OTP_VSN=28.5.0.7-2`. The gate reads only these declared pins (via `required_otp`/`OtpPin` in `src/workloads/emqx.rs`); it never assumes checkout layout.
* **The exact available runtime.** `erl -noshell -eval 'io:format("~s ~s~n",[erlang:system_info(otp_release), erlang:system_info(version)]), halt().'` → `29 17.1`, resolved at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl` (the driver reports both the major and the erts version in the note).
* **The mechanism (why a major-version mismatch is fatal here).** The umbrella build aborts in the `:ezstd` dependency's NIF makefile, which regenerates its `env.mk` with the idiom `erl -noshell -s init stop -eval '…'`. On OTP 29 `init stop` halts **before** the `-eval` runs, so the idiom writes nothing — demonstrated directly on this host: it exits `0` with **no output at all** (on OTP 28 it prints the line). `mix compile` then fails with `No rule to make target '…/env.mk'`. EMQX documents the same constraint in its own `scripts/ensure-rebar3.sh`, whose `case ${OTP_VSN}` accepts only `25*`/`26*`/`27*`/`28*`.
* **Why not "just override the pin".** The only OTP-28 binding in the store (`erlang-28.5.0.7`) cannot be paired with the OTP-29-built **Elixir 1.20.4** / `mix` that the umbrella build's `mix` stage needs, and the pin lives in the **read-only** target checkout (editing the pinned version would be changing the project under test — not an honest measurement). Swapping the whole toolchain is out of this crate's scope.

Recorded in `results/m10-bench-emqx.json` (the skip is immediate, before any build work).

### 4.4 `cowboy` — hello_world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s — `--full`)

`copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk git-fetches cowlib/ranch), starts the release and drives it with the in-process keep-alive loader.

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 39 511.74 ms |
| requests | 1 051 576 |
| **throughput** | **210 250.49 req/s** |
| errors | 0 |
| latency min / p50 / p90 | 0.021357 / 0.063634 / 0.099444 ms |
| **latency p99** | **0.301994 ms** |
| latency max | 3.622108 ms (n = 20 000) |
| peak RSS | 171 356 160 B (163.4 MiB) |

**vs m9** (also `--full`, 16 conns × 5 s): p50 essentially unchanged (0.063634 vs 0.063911), throughput −4.5 % (210 250 vs 220 144), p99 +9 % (0.30199 vs 0.27638). The in-process loader and the server share one contended machine and nothing about the release build or the loader changed, so this is **host-load variation, not a server change** (§7.3). (`m6-bench-cowboy.json` is a `--quick` run, not comparable.)

### 4.5 `erlc_compiler` — `erlc` over the stdlib + Elixir corpus slice

| metric | value |
|---|---|
| wall | 6 785.46 ms |
| files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| **throughput** | **8.842 files/s** (**6 853.33 LOC/s**) |
| peak RSS | 135 405 568 B (129.1 MiB) |
| notes | `corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)` |

The `files_per_sec`/`loc_per_sec` rates are computed from the files/LOC `erlc` actually **produced**, not from what was attempted — so a partial run cannot overstate them. Here all 60 attempted files produced a `.beam`, so produced = attempted. The rate is ~10 % below m9's (9.844 files/s) with the **same 60/60 files and the unmodified system `erlc`**, so the move is host-load-attributable (§7.3).

### 4.6 `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

Measures the **installed** `elixirc` (the in-repo checkout is unbuilt and read-only) over a staged `.ex` corpus (`--full` ⇒ up to 200 files).

| metric | value |
|---|---|
| wall | 47 806.41 ms |
| files attempted / `.beam` produced | 105 / **223** (97/105 compiled) |
| **throughput** | **4.665 files/s** (**2 342.03 LOC/s**) |
| compiler startup | 262.05 ms |
| peak RSS | 208 289 792 B (198.6 MiB) |
| notes | `batch compile failed; per-file fallback (version skew)`; `compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`; `223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)` |

**`files_attempted` (105) < `files_compiled` (223) is expected, not a bug:** one Elixir source may define several modules, so the produced `.beam` count can exceed the source count. The throughput rate uses the produced count.

## 5. Workloads that did **not** run in the m10 measurement run — verified reasons

| workload | status | verified reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`) while the host `erl` is OTP 29.1.1 / erts-17.1; the `:ezstd` `env.mk` make-target's `erl -noshell -s init stop -eval …` idiom produces nothing under OTP 29, so `mix compile` aborts. Not satisfiable without a full OTP-28 toolchain swap (§4.3). No number is fabricated. |
| every `SysOnly` workload scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/bootstrap an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. (Not exercised as a row in m10 — the m10 sys-only runs used `--runtime sys` — but it is the harness behaviour.) |

## 6. Reproducibility, stability and known measurement caveats

### 6.1 The host is shared and loaded, and the two runtimes are measured in different windows
The 1-min `/proc/loadavg` across the m10 session ran from **5.43 to 27.43** against a 14-core host (§1, `results/m10-load-record.md`); the primary full runs happened at the quieter end (5.43 → 7.59 and 7.94 → 8.03) but the PLB pass ended at 27.43. The runner is **runtime-major** (`sys` rows first, then `rust` rows), so under a drifting load the two sides see different conditions. Loadavg is host-wide and does **not** move monotonically with the measured figures (§7), so prefer ratios, prefer the startup-cancelled rows, and always record `/proc/loadavg` alongside an absolute figure.

### 6.2 `beam_rt` resolves its code path natively from the filesystem
The runtime locates its system `ebin` directories (stdlib, kernel, erts) with a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`: `$ERTS_STDLIB_DIR` when set, else the standard OTP installation layout under the `erl` on `$PATH` — *resolved to a path, never executed* — then this executable's ancestors, `$ERL_TOP`, and standard prefixes). The system `erl` is only *run* as a last resort, for an installation that layout cannot see. That is why rust startup is ~15–16 ms per process. Setting `$ERTS_STDLIB_DIR` still short-circuits the probe; `bench` never sets it (the harness is a pure driver/observer and never mutates the runtime's environment beyond argv). The per-element/per-call metrics cancel whatever startup cost remains by construction.

### 6.3 `elixir_compiler` measures the installed `elixirc` 1.20.4
against the unbuilt, read-only 1.21.0-dev checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the artifact.

### 6.4 `cowboy` re-fetches cowlib/ranch in a fresh scratch copy on every run (network)
which is why its `build_ms` (~39.5 s) dominates the workload.

### 6.5 Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`)
Erlang's own `erlang:memory(total)` is a **separate** metric (§3.2) and is never conflated with it.

### 6.6 `foreign` / `list_walk` / `copy_bound` / `gc_bound` `wall_ms` are not comparable across runtimes
Each side runs a different `N`. Compare the startup-cancelled marginals (`ns_per_call`, `ns_per_element`, `ns_per_op`) and `startup_ms`.

### 6.7 The rust peak RSS of `foreign` / `list_walk` / `copy_bound` / `gc_bound` still grows with the loop count `N` — it is **not** a like-for-like memory comparison
`beam_rt` has a process-heap collector driven from `crates/beam_rt/src/context/gc.rs` at the scheduled-slice heap-admission point, and the m10 BIF-result copy-out + pooled-heap rewind (`crates/beam_rt/src/context/calls.rs`) bounds the **BIF-churn** shape — `gc_bound`'s rust peak RSS is now **15.40 MiB p50** (was 38.78) and `list_walk`'s **15.32 MiB** (was 15.36). But the **`copy_bound` `ets_copy`** row still reaches **90.30 MiB** at N = 100 000 vs sys 66.26 MiB (it was 75.93 MiB in m9), and `foreign`'s rust RSS (73.8–76.4 MiB for the two `cowboy_bstr` cases) still tracks its N. `beam_rt` peak RSS therefore still scales with `N` on those paths.
**Consequence:** use `slice` (§3.3, one call per process, `N`=1) as the comparable per-process metric, and treat the loop workloads' rust RSS as loop-length-dependent (§8 item 5).

## 7. Deltas vs the previous run (`m9`, and `m8`/`m6`/`m5`)

Every figure below is quoted from the committed artifacts: `m10-*` (2026-10-13 23:06–23:15 UTC, commit `b4593818b3`, full-pass load 5.43 → 7.59 / 7.94 → 8.03 / 21.12 → 10.30), `m9-*` (2026-10-13 13:25–14:10 UTC, load 11.01 → 6.02 / 4.68 → 4.43), `m6-*` (2026-10-11, cross-runtime load ≈414–440), `m5-*` (2026-10-09, load ≈90–153) and `m4-*` (2026-10-08, load 4.77 → 11.59). **The runs sit at very different — and non-monotonic — host loads.**
The tool-generated two-file diffs are archived: `results/m10-compare-vs-m9-xrun.txt` and `results/m10-compare-vs-m9-external.txt`.

**Cross-run comparability.** Only the **load-invariant** rows are safe to compare across runs: the startup-cancelled marginals and, above all, the **deterministic counters** (`*_ops`, `*_elements`). The absolute wall / startup / throughput rows are **not** cross-run comparable — they moved in *both* directions between runs — which is why `/proc/loadavg` is treated as a weak proxy throughout this document.

### 7.1 m10 vs m9 — the cross-runtime marginals (rust side)

Rust `beam_rt` cells, m9 → m10 (mean; bigger-is-better negative deltas are improvements):

| metric (mean) | m9 | m10 | move |
|---|---|---|---|
| `baseline` `startup_ms` | 17.2648 | **15.5243** | −10.1 % (rust faster) |
| `baseline` `vm_memory_total_bytes` | 805 458 | 805 458 | unchanged |
| `list_walk` `ns_per_element` | 158.8297 | **47.3880** | **−70.2 %** |
| `copy_bound` `etf_roundtrip` `ns_per_op` | 77 137.9386 | **9 838.0045** | **−87.3 %** |
| `copy_bound` `ets_copy` `ns_per_op` | 3 391.3562 | **3 267.8549** | −3.6 % |
| `copy_bound` `send_recv` `ns_per_op` | 165 897.2982 | **79 518.1199** | **−52.1 %** |
| `gc_bound` `churn` `ns_per_op` | 179 192.6512 | **147 194.3719** | **−17.9 %** |
| `foreign` `bstr_to_lower` `ns_per_call` | 19 606.7325 | **17 789.3946** | −9.3 % |
| `foreign` `bstr_capitalize_token` `ns_per_call` | 24 782.3962 | **22 566.6138** | −8.9 % |
| `foreign` `emqx_base62_encode` `ns_per_call` | 10 283.3025 | **16 601.0457** | **+61.4 % (worse)** |
| `foreign` `emqx_base62_decode` `ns_per_call` | 17 698.4921 | **16 325.9957** | −7.8 % |
| `gc_bound` `churn` peak RSS p50 | 38.78 MiB | **15.40 MiB** | **−60.3 %** |
| `list_walk` peak RSS p50 | 15.36 MiB | 15.32 MiB | unchanged |

The **sys (system ERTS)** side is unchanged within load: `baseline` `startup_ms` 126.5878 → 114.3603 (host variation), `foreign` `ns_per_call` 222.49–371.84 → 210.88–339.42, `list_walk` `ns_per_element` 2.4238 → 2.1431, `copy_bound` `ns_per_op` 239.95–1 450.39 → 240.47–1 453.98, `gc_bound` `churn` 4 248.40 → 4 364.91.

### 7.2 Real, code-attributable changes since m9

* **`copy_bound etf_roundtrip` fell 87.3 % on the rust side** (77 137.94 → 9 838.00 ns/op) — the ETF atom-table-clone removal (`beam_bifs`'s decode scratch image; `CHANGE.md` M10). The in-crate codec micro-bench (`examples/etf_bench.rs`) reports **121.3 → 5.69 µs/op (21.3×)**; the `bench` workload's **7.8×** is the *whole iteration* (payload construction + `term_to_binary`/`binary_to_term` + the bound-variable equality check), so the two figures are **not** the same quantity — state the measurement shape when quoting either.
* **`list_walk` per-element fell 70.2 % on the rust side** (158.83 → 47.39 ns) — the baseline JIT's Y-slot (stack) arithmetic + continuation sub-blocks plus the single-resolution `call_ext` seam and the per-artifact fingerprint memo (`beam_engine`, §7 in `CHANGE.md` M10). The tier table (§3.9) shows the same change from the other side: the default tier is now **3.84× cheaper** than the interpreter on this shape.
* **`gc_bound` rust peak RSS fell 60.3 %** (38.78 → 15.40 MiB p50) with the churn wall cost −17.9 % — the BIF-result copy-out + pooled-heap rewind (`beam_rt`; `CHANGE.md` M10). The in-crate figure is the flat **~19.5 MiB over a 50× N range** (the bench row is at one N).
* **`send_recv` rust fell 52.1 %** (165 897 → 79 518 ns/op); it remains the dearest copy path and the one row where the default tier is *dearer* than the interpreter (§3.9, §8).
* **`beam_engine` call seams:** the single-resolution `call_ext` seam and the fingerprint+address memo; `walk/2` 5.09×, `kernel/2` 5.93×, `sum/1` 0.80–0.88× → 1.65–1.67×, `cross/2` 0.73× → 1.03× (in-crate, `crates/beam_engine/MEASURED.md`).
* **`plb` runs** (the version-aware tool resolution; §4.1) — `ok` at 70 841.43 ms.
* **`beam_process`** gained the signal-family-aware exit policy (`ExitFamily::Explicit | Linked`); not measured by this driver (its surface is the differential gate).

### 7.3 Load-attributable / not comparable

* **`baseline` startup, `sys` side.** m9 mean 126.59 ms → m10 mean 114.36 ms — lower, at a much lower recorded loadavg; host variation, not a code change.
* **Cowboy throughput.** p50 unchanged (0.063634 vs 0.063911), throughput −4.5 %, p99 +9 % — host load, per §4.4.
* **Compiler throughput / `khepri` wall.** `erlc_compiler` −10.2 % (9.844 → 8.842 files/s, same 60/60 files), `elixir_compiler` +1.9 % (4.579 → 4.665 files/s, same 105/223), `khepri` +1.3 % (77 755.91 → 78 762.66 ms). All ran the unmodified system toolchain, so these moves are not code-attributable.

### 7.4 Unchanged / still-open

* **`baseline` `vm_memory_total_bytes` (rust)** — m9 805 458 B → m10 805 458 B (the two runtimes still report different quantities, §3.2).
* **`foreign` peak RSS still scales with `N`**, and `copy_bound`'s `ets_copy` row now reads **90.30 MiB** (was 75.93) at N = 100 000 (§6.7, §8 item 5).
* **`list_walk` and `gc_bound` rust peak RSS are now *below* the oracle's** (15.3/15.4 MiB vs 66.4/66.8 MiB), but they are still loop-length-dependent rows, not like-for-like.
* **The per-unit execution-throughput gap is the stable cross-runtime story.** `foreign` rust/sys: `m4` (quiet) 53.8×–72.0×; `m5` 53.1×–96.0×; `m6` 74.9×–131.4×; `m9` 46.2×–74.2×; `m10` **52.4×–78.7×**. `list_walk`: `m5` 66.6× → `m6` 108.7× → `m9` 65.5× → `m10` **22.1×** (the m10 JIT work is visible here).

## 8. Fresh ranked bottleneck list (from the m10 data)

Derived from the m10 measurements above (each item: measured m10 cost → where to look → expected win). Each ratio names **both sides**: `rust` = `beam_rt` (default baseline-JIT tier) vs `sys` = system ERTS, unless the row is explicitly a Rust-vs-Rust tier row.
The previous (M9) list drove the M10 batch: its ETF-atom-clone, unbounded-BIF-churn-RSS and baseline-JIT-Y-slot/continuation items are **closed** (§7.2). What remains, ranked by measured headroom:

1. **Per-unit Erlang execution throughput — the dominant gap (13.6×–78.7× vs system ERTS).** Every startup-cancelled marginal sits here: `list_walk` 47.388 vs 2.1431 ns/element (22.1×), `copy_bound` 3 267.85–79 518.12 vs 240.47–1 453.98 ns/op (13.6×–54.7×), `foreign` 16 326–22 567 vs 210.9–339.4 ns/call (52.4×–78.7×), `gc_bound` 147 194 vs 4 364.9 ns/op (33.7×).
   *Lead:* the engine step loop (`erts_rust/crates/beam_engine/src/machine/drive.rs`) and baseline-JIT block coverage (`erts_rust/crates/beam_engine/src/jit/baseline/emit/`), plus the host BIF seam (`erts_rust/crates/beam_rt/src/context/calls.rs::call_registered`, ~line 122) — the engine's own side is only ~11–12 ns of the per-call cost, so ~99.85 % is **host-side** (`crates/beam_engine/tests/bif_cost.rs`).
   *Expected win:* the largest single lever in the suite — a 20 % cut on the per-unit cost is worth more than any other item; the M10 JIT work already took `list_walk` from 65.5× to 22.1×.
2. **Baseline-JIT message-passing regression: the default tier is 2.15×–3.31× *dearer* than the interpreter tier on `send_recv`.** Rust-vs-Rust only: default **79 518.12** vs interpreter **36 969.39** ns/op (full run) = **2.15×**; a focused `--quick` re-run reproduced it at **97 517.77 vs 29 492.93** = **3.31×**. `PERF.md`'s own `CURRENT` snapshot recorded the same direction at the previous HEAD (**1.456× worse**), so it is repeatable, not noise.
   *Lead:* `erts_rust/crates/beam_engine/src/jit/baseline/runtime.rs:338` (`jit_step_one`, the per-instruction deopt) and `erts_rust/crates/beam_engine/src/jit/baseline/mod.rs:145` — a `send`/`receive` the compiled block cannot lower deopts to the interpreter step, so each op pays a block-boundary round trip *on top of* interpreted execution; `emit/select.rs` is the receive-dispatch emitter.
   *Expected win:* bring the native tier to parity with the interpreter on this shape (≈2× on `send_recv`, 79.5 → ~37 µs/op) and remove a tier *regression* (message-passing should be the JIT's strong case, not its weak one).
3. **`gc_bound` churn is still 33.7× dearer (147 194 ns/op) — the memory half is fixed, the allocation wall cost is untouched.** m10 rust peak RSS is now flat/low (**15.40 MiB** p50, was 38.78), but the per-op churn cost only fell 17.9 %.
   *Lead:* the copy-out + rewind seam (`erts_rust/crates/beam_rt/src/context/calls.rs::move_result_into_process_heap`, ~line 674; the rewind guard in `call_registered`) — every `churn` iteration builds three containers and each container-returning BIF result is now copied into the process heap.
   *Expected win:* a cheaper copy-out (bulk word copy, avoiding a re-walk) applies to *every* container-returning BIF, so it should show up on `churn`, `ets_copy` and the module-call cases together.
4. **`foreign` `emqx_base62:encode/1` regressed +61.4 % (10 283.30 → 16 601.05 ns/call)** while the other three `foreign` cases improved 8–9 %. m10 ran at a *lower* host load than m9, so load does not explain it.
   *Lead:* the same copy-out seam (`calls.rs::move_result_into_process_heap`) — m9 returned the container without copying; the other three cases either return small/immediate results or their prior cost masks the delta. The alternative explanation is that m9's encode figure was anomalously low (it was the one case far below the others' band), in which case m10's is a *correct* re-measurement.
   *Expected win:* ~6 µs/call on encode; more importantly it calibrates the copy-out cost per call. **Confirm with a controlled in-crate A/B (one binary, copy-out on/off) before attributing** — the bench row alone cannot separate the two hypotheses.
5. **`ets_copy` peak RSS still grows with `N`: rust 90.30 MiB at N = 100 000 vs sys 66.26 MiB** (it was 75.93 MiB in m9 — it grew). The churn fix did not cover the ETS insert/lookup loop.
   *Lead:* `erts_rust/crates/beam_rt/src/context/calls.rs::call_registered`'s rewind guard plus the ETS adopt path — `ets:lookup` hands back a copied result list the rewound pooled heap no longer holds, so the loop accumulates on the process heap.
   *Expected win:* make `ets_copy`'s per-op memory flat like `gc_bound`'s; it is the last loop whose peak RSS *rises* above the oracle's.
6. **`slice` system-module per-process cost rose 7–44 % vs m9 while the fixture floor improved.** Rust walls: `gb_trees_size` 18.13 → 25.12 (+38.5 %), `base64_encode_size` 19.44 → 26.68 (+37.2 %), `ordsets_from_size` 18.09 → 26.15 (+44.6 %), `queue_from_size` 22.40 → 26.00 (+16.1 %), `string_str_length` 41.29 → 48.12 (+16.5 %); the fixtures moved the other way (`add` 18.74 → 15.38, `fib` 18.06 → 16.22, `pick` 17.27 → 16.10). m10 measured at a *lower* load than m9, so load does not explain it.
   *Lead:* the system-module entry path — the code-path probe (`erts_rust/crates/beam_rt/src/codeserver.rs`) and the lazy per-function compile (`erts_rust/crates/beam_engine/src/jit/baseline/code.rs`). Every case that calls a system module pays it; the pure fixture cases do not. Alternatively a per-call cost added by the copy-out seam.
   *Expected win:* restore the m9 floor (~6–10 ms/case). These are single-call-per-process rows, so start with an A/B confirmation (interleaved default vs `BEAM_RT_TIER=interpreter`, 25 samples) before treating it as a regression — the sample is startup-bound and noisy.
7. **`send_recv` absolute per-op cost is high in both Rust tiers** (79 518 ns/op default, 36 969 interpreter) vs the oracle's 1 453.98 (54.7×) — listed separately from item 1 so the message-passing surface is not lost inside the generic per-unit item.
   *Lead:* `erts_rust/crates/beam_message`'s copy engine and `erts_rust/crates/beam_process`'s signal delivery, reached through the engine's `send`/`receive` seams.
8. **Still open from `PERF.md`'s post-M9 snapshot: the two one-shot cold-start rows** (`slice` `string_str_length` **1.295×** and `queue_from_size` **1.217×** vs the interpreter; `PERF.md` §"tier effect"). m10 shows the same shape from the bench side: `string_str_length` is the only slice case well above 26.7 ms (48.12 ms). It is bounded (lazy per-function compile, ~1.2–1.3×), so it is a low-priority refinement, not a hole.

## Honesty audit — measurement-integrity notes

No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always `failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`, `src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`/non-doc `unreachable!`, no `#[ignore]`.

- **`foreign` classifies a staging failure honestly** (`src/workloads/foreign.rs`: `StageError` + `stage_outcome`): a checkout whose foreign `.erl` sources are genuinely absent ⇒ `skipped(sources-missing)`, while a failure to compile the sources we *did* stage (the embedded driver or a present foreign source) ⇒ `failed(module-compile-failed)` — matching the workloads whose sources are ours (`list_walk`/`copy_bound`/`gc_bound`). `slice` treats a failed fixture rebuild as `skipped(fixture-missing)`.
- **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` are computed from the *produced* counts**, so a partial `erlc` run cannot overstate throughput next to `files_compiled`. `elixir_compiler` likewise uses the produced count.
- **A version-constrained tool is resolved by version, and the choice is auditable.** `plb` calls `tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` (min `9`), which probes every candidate's `--version` and selects the greatest satisfying one. The resolved binary + version + source and every rejected candidate's reason are recorded in the row's `notes` — in **both** the success and the build-failure branch (§4.1). `khepri`'s `runtimes[]` version describes the driver's oracle, not necessarily the Erlang the store `rebar3` escript chose (§4.2).
- `proc.rs`'s drain-thread join (`h.join().unwrap_or_default()`) is infallible **by construction** — the drain threads run only `read_to_end` + a lossy UTF-8 conversion, neither of which can panic — so the `unwrap_or_default` masks nothing real; the invariant is documented at the call site.
- `baseline.rs`'s docs correctly state that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its `MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`; `is_capability_gap` survives only as a fallback classifier.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` startup-cancelled methodology is implemented exactly as this document states (startup-cancelled `(mean wall@N − mean wall@0) / N`). The runtime-side hot-path figures (registered-BIF dispatch ≈ 79–96 ns/call; `pb:tr/2`'s `trunc/1` ≈ 76 ns/iter) are `beam_rt` measurements (`../../crates/beam_rt/PERF.md`, `../../crates/beam_rt/TRACE_AND_PERF.md`), **not** produced here.
- **`PROBE`/`m10` pass integrity:** the driver and `beam_rt` were built from commit `b4593818b3a27f47e3b682a1a50fbfcc0a1785fe`; every run's stdout was checked against the deterministic constant on both sides; the tier control is a separate artefact (`m10-bench-xrun-interp.json`) so the two Rust tiers cannot be conflated with the `sys` column; all committed artifact paths are normalised (`runtimes[].program` → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`).

**Reproducibility note (PLB / tool resolution).** `plb`'s outcome is *tool- and environment* state, not repository state. The resolver now picks the **greatest `dotnet` ≥ 9.0** across `PATH` + `$BENCH_TOOL_PATH` + the Nix store, so `plb` runs wherever any 9.x+ SDK is reachable — even with an older `dotnet` first on `PATH`. `khepri` needs only the `rebar3` store entry and runs as-is.

## Archived artifacts

| file | contents |
|---|---|
| `results/m10-bench-xrun.json` | **current cross-runtime run**: `--full --runtime all`, the six `Runtimes::Both` workloads × both runtimes, 12/12 rows `ok` (load 5.43 → 7.59, wall 2m36s) — the source of §3 |
| `results/m10-bench-xrun-interp.json` | **the tier-effect control**: the same six workloads, `rust` only, `BEAM_RT_TIER=interpreter`, 6/6 rows `ok` (load 7.94 → 8.03, wall 41s) — the source of §3.9. **Rust-vs-Rust only; no `sys` rows.** |
| `results/m10-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside `m10-bench-xrun.json` (69 numeric rows; the tool diffs the **mean**) |
| `results/m10-compare-tier-effect.txt` | `bench compare m10-bench-xrun-interp.json m10-bench-xrun.json` — Rust interpreter → Rust default (`a` = interpreter) |
| `results/m10-compare-vs-m9-xrun.txt` / `results/m10-compare-vs-m9-external.txt` | the tool's two-file diffs against the m9 record — the source of §7 |
| `results/m10-bench-external.json` | the external sys-only targets in one `--full` report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri` (§4.2, §4.4–§4.6; load 21.12 → 10.30, wall 3m00s) |
| `results/m10-bench-plb.json` | `plb` → `ok`, `wall_ms` 70 841.43, store 9.0.318 resolved from `PATH` (§4.1) |
| `results/m10-bench-emqx.json` | EMQX → `skipped(emqx-requires-otp-28)` (§4.3) |
| `results/m10-load-record.md` | the m10 pass: commit measured, exact commands, the `/proc/loadavg` before/after each run, per-workload status, per-case detail and the tier-effect table |
| `results/m10-tool-targets-notes.md` | the factual per-target status/blocker notes and the environment findings |
| `results/m9-*` | the **previous** run (2026-10-13 13:25–14:10 UTC): the focused cross-runtime report + tier/instrumentation notes, the per-target reports, `m9-compare-cross-runtime.txt`, and the `m9-load-record.md`/`m9-tool-targets-notes.md` records — **regression baseline for §7** |
| `results/m6-*` / `m5-*` / `m4-*` | the runs before that (2026-10-11 / 2026-10-09 / 2026-10-08), kept as earlier regression baselines |

The archived copies are the harness output except for two mechanical normalisations that strip environment-specific absolute paths: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` is replaced with `$TMPDIR/`. **No metric value is altered.**
