# Rust-ERTS benchmark results — current measurement run (m9)

Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured, never modelled**.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-13, 13:25–14:10 UTC** (the span of the artifacts' own `generated_at_unix_ms`: `m9-bench-plb.json` 13:25:38 → `m9-bench-emqx.json` 14:09:45).
Raw harness JSON is committed under `results/` as `m9-*`; the previous runs are kept as `m6-*` (2026-10-11), `m5-*` (2026-10-09) and `m4-*` (2026-10-08) and are the regression baselines for the delta section (§7).
A **later same-session smoke sub-run (2026-10-13, 14:49–14:53 UTC)** re-checked the two tool-driven targets after the tool-resolution fix (§4.1–§4.3): `plb` now **runs** and `emqx` keeps its skip. Those are smoke runs (one workload each, `--quick`), archived as `m9-bench-plb-smoke*.json` / `m9-bench-emqx-smoke.json`; they are **not** a re-measurement of the m9 figures above.

> **This is a shared, usually-loaded host** (§1, §6.1): absolute latencies/throughputs move several-fold with load, so **ratios are the signal** and a single contended sample is never a constant. §4.1's two smoke rows illustrate it directly — the same code, run 2 minutes apart at 1-min loads 6.2/12.5 and 10.6/34.7, gave 73.6 s vs 72.6 s wall. Do **not** re-measure the m9 headline figures to "refresh" them; a full re-measure is a later, deliberate milestone (the engine/BIF/allocator work has not landed yet).

## Verdict

**(a) / (b) Which targets run, which skip.** Five of the six external target projects run; one (`emqx`) skips with a verified toolchain reason (never a fabricated number). `plb`, which the m9 measurement run recorded as `failed`, now **runs** after the tool-resolution fix (§4.1) — each outcome below carries its own date, because the target-resolvability is tool/environment state, not repository state.

| target project | runs? | result / verified reason |
|---|---|---|
| `khepri` (tree DB) | **runs** | `ok` — `rebar3 eunit` wall 77 755.91 ms (§4.2) |
| `cowboy` (HTTP server) | **runs** | `ok` — 220 143.53 req/s, p50 0.0639 ms / p99 0.2764 ms (§4.4) |
| `erlc_compiler` (Erlang compiler) | **runs** | `ok` — 9.844 files/s over the stdlib+Elixir corpus (§4.5) |
| `elixir_compiler` (Elixir compiler) | **runs** | `ok` — 4.579 files/s over 105 sources / 223 `.beam` (§4.6) |
| `plb` (Benchmarks Game kernels, .NET) | **runs** | `ok` (2026-10-13 14:49–14:53 UTC smoke) — version-aware tool resolution picks the store's `dotnet` 9.0.318 and builds the `net9` env (§4.1). *The m9 measurement run itself recorded `failed(plb-build-failed)`: the `dotnet` on `PATH` then was 8.0.425* |
| `emqx` (MQTT broker) | **skips** | `skipped(emqx-requires-otp-28)` — EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`), the host `erl` is OTP 29.1.1 / erts-17.1; not satisfiable without a full OTP-28 toolchain swap (§4.3, §5) |

The six workloads the Rust runtime itself can host (`Runtimes::Both`) — `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound` — all ran: **12/12 rows `ok`** (0 skipped, 0 unsupported, 0 failed). Every other workload is `SysOnly` and would report `skipped(runtime-unsupported)` under `rust` (§5).

**(c) Ratio to the system-ERTS oracle, per cross-runtime workload.** `rust/sys` is the Rust cost as a multiple of the system cost (`<1` ⇒ Rust cheaper, `>1` ⇒ Rust dearer). Values are the m9 `xrun` (§3, `--full`, 1-min load 11.01 → 6.02).

| workload | surface | headline metric | sys | rust | rust/sys | reading |
|---|---|---|---|---|---|---|
| `baseline` | startup / CLI | `startup_ms` (mean, whole process) | 126.59 ms | 17.26 ms | **0.136×** | Rust **7.33× faster** per process |
| `slice` (9 programs) | compute | `case_*_wall_ms` (mean, per case) | 128.47–139.99 ms | 17.27–41.29 ms | **0.123–0.309×** | Rust **3.24×–8.11× faster** |
| `foreign` (Cowboy/EMQX source) | compute / `.beam` | `case_*_ns_per_call` | 222.49–371.84 ns | 10 283.30–24 782.40 ns | **46.2×–74.2×** | Rust **46.2×–74.2× dearer** per call |
| `list_walk` | dispatch | `case_list_walk_ns_per_element` | 2.4238 ns | 158.8297 ns | **65.5×** | Rust **65.5× dearer** per element |
| `copy_bound` (ETF / ETS / send-recv) | Msg / copy | `case_*_ns_per_op` | 239.95–1 450.39 ns | 3 391.36–165 897.30 ns | **14.1×–118.5×** | Rust **14.1×–118.5× dearer** per op |
| `gc_bound` (allocation churn) | GC | `case_churn_ns_per_op` | 4 248.40 ns | 179 192.65 ns | **42.2×** | Rust **42.2× dearer** per op |

- **The two directions are both real and answer different questions.** Rust's *whole-process cost* is far lower (a `beam_rt` process is small and starts in ~17–22 ms), while the *per-unit execution cost* of Erlang code under `beam_rt` is far higher (the per-unit execution-throughput gap: **14×–119×**, §3.8). Startup is the trustworthy whole-process comparison; the startup-cancelled marginals are the per-unit execution-throughput signal.
- **The one-shot cold start is bounded by per-function lazy compilation** (§3.9). A `slice` sample is a single call in a fresh process, and a module-graph-heavy case whose first entry pulls in a large Erlang module graph (`string_str_length`-style) pays the first-entry compile cost, not a whole-module compile.
- **Peak RSS is a like-for-like cross-runtime comparison only for `slice`** (one call per process). For `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS grows with the per-runtime loop count `N` and is **not** comparable (§6.7); `slice` shows rust **2.61× lower** than sys.
- **Load caveat (read before quoting any absolute number).** At the primary **full** cross-runtime run the 1-min `/proc/loadavg` was 11.01 → 6.02, and the primary **full** external-target run 4.68 → 4.43, but the host is shared and earlier passes in the same session saw 6.66 → 28.95 and 27.75 (§1). Absolute latencies still move several-fold with contention and are **not comparable across runs**; prefer the ratios above, the startup-cancelled marginals and the deterministic counters.

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

> The benchmark host is a **shared 14-core machine** and was heavily oversubscribed during m9 at times.
> The primary (**full**) runs were taken at a comparatively quiet moment — `/proc/loadavg` 1-min **11.01 → 6.02** (cross-runtime) and **4.68 → 4.43** (external targets) — but earlier passes in the same session saw **6.66 → 28.95** and a **27.75** PLB attempt (§1 records every reading, and `results/m9-load-record.md` records the exact sequence).
> Every *absolute* latency / wall / throughput below is still inflated by CPU contention and is **not comparable across runs**. Prefer **ratios**, the **startup-cancelled per-op / per-call / per-element marginals** (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counters** (`*_ops`, `*_elements`).
> `/proc/loadavg` is host-wide and a weak proxy — it does **not** move monotonically with the measured figures (§7) — so record it, but never infer a slowdown from it alone.
> The load was recorded **before and after every run** (§1/`results/m9-load-record.md`) — mandatory here.

## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (UTC) | 2026-10-13 13:25–14:10 UTC |
| Host / kernel | Linux 7.2.9 (`#1-NixOS SMP PREEMPT_DYNAMIC`), `x86_64` |
| Distribution | NixOS 26.11 (Zokor) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 65 317 432 kB (62.29 GiB) |
| Host load (1-min `/proc/loadavg`) | full cross-runtime `xrun` **11.01 → 6.02**; full external targets **4.68 → 4.43**; earlier quick passes **11.98 → 12.80**, **7.35 → 7.06**, **6.66 → 28.95**, PLB **27.75 → 27.75**, EMQX **4.11** (per-run table in `results/m9-load-record.md`) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl` at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`, **OTP 29.1.1 / erts-17.1** (`otp_release` = `"29"`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, resolved from the Nix store by the driver's **store scan** (**not** on `PATH`); its shebang runs it under **Erlang/OTP 28** — see the honesty note on `runtimes[]` version reporting |
| `dotnet` | **9.0.318, on `PATH`** (`/run/current-system/sw/bin/dotnet` → store `dotnet-sdk-wrapped-9.0.318`), **as of the 14:49–14:53 UTC smoke sub-run**. During the m9 measurement run (13:25–14:10) the `PATH` `dotnet` was **8.0.425**, which shadowed the store's 9.0.318 SDK (the old policy consulted `PATH` before the store scan) and made `plb`'s `net9.0` build abort with `NETSDK1045` (§4.1). The store holds both an 8.0.425 and a 9.0.318 SDK; the driver now resolves the version-constrained tool by **version**, not source order (§4.1, §5) |
| `clang` | **21.1.8**, on `PATH` |
| `$BENCH_TRY_NIX` | not needed and not set for the m9 runs — the store scan resolves `rebar3` locally (§4.2) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` |
| `/usr/bin/time` | **absent** — the driver's own timers and `/proc/<pid>` `VmHWM` polling are used |

Both runtimes resolve and pass the harness self-check (`supported: true`).
The driver and the runtime were both built from this tree (`--release -p beam_rt -p bench`).

Per-run `/proc/loadavg` (1-min value first; the full sequence, with the exact commands, is in `results/m9-load-record.md`):

| run | before | after |
|---|---|---|
| quick cross-runtime pass | `11.98` | `12.80` |
| quick compilers pass | `7.35` | `7.06` |
| quick cowboy + khepri pass | `6.66` | `28.95` |
| PLB quick attempt (fails in ~1.3 s) | `27.75` | `27.75` |
| EMQX quick pass (skips immediately) | `4.11` | — |
| **FULL cross-runtime pass** (`m9-bench-xrun.json`, wall 2m44s) | `11.01` | `6.02` |
| **FULL external-target pass** (`m9-bench-external.json`, wall 3m00s) | `4.68` | `4.43` |

## 2. Exact commands

Build the driver and the runtime:
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
```
Probe status:
```sh
erts_rust/target/release/bench list --json
```
The **focused cross-runtime run** — all six `Runtimes::Both` workloads, `--full`, both runtimes → `results/m9-bench-xrun.json` (wall 2m44s):
```sh
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m9-bench-xrun.json --verbose
```
Cross-runtime diff (archived as `results/m9-compare-cross-runtime.txt`; the tool diffs the **mean**):
```sh
erts_rust/target/release/bench compare \
    erts_rust/tools/bench/results/m9-bench-xrun.json \
    > erts_rust/tools/bench/results/m9-compare-cross-runtime.txt
```
The **full sys-only external-target run** (one report, four workloads; wall 3m00s) and the two remaining single-target observations:
```sh
erts_rust/target/release/bench run --full  --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m9-bench-external.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m9-bench-plb.json  --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m9-bench-emqx.json --verbose
```
Crate tests: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench`.

Profile semantics: `--full` ⇒ `baseline` 50 startup samples / 5 compute samples, `slice` 25 timed runs/case, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200 files.
`--quick` ⇒ `baseline` 10 / 5, `slice` 5, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 3, `cowboy` 8 connections × 2 s.
The per-runtime loop count `N` in the `foreign`/`list_walk`/`copy_bound`/`gc_bound` cases is **fixed per case** (not profile-dependent), so the `--full` sample counts only widen the estimate, never the loop length (§3.4–3.7).
**Run order is runtime-major**: all `sys` rows execute first, then all `rust` rows — under a *drifting* host load the two sides are measured in different windows (§6.1).

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

`sys` = OTP 29.1.1 with the JIT. `rust` = `beam_rt`, released: a working runtime whose per-process execution tier **defaults to the native baseline JIT** (`JitMode::Auto` ⇒ `Tier::BaselineJit` on the process's first entry, remembered across pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the only kill switch), with anything outside the modelled `genop.tab` surface returning a typed `unsupported`/`BeamError` rather than a wrong result. Its runtime path compiles **per function, lazily** on a function's first entry (`../../crates/beam_engine/src/jit/baseline/code.rs`) — never whole-module; the tier effect and the one-shot cold start are in §3.9.
All tables in this section come from the **focused `xrun`** (`results/m9-bench-xrun.json`, `--full --runtime all`, 1-min load 11.01 → 6.02), which runs the six cross-runtime workloads back to back: **12/12 rows `ok`**.

### 3.1 Startup latency — `baseline` (n = 50, ms)

| stat | sys | rust | rust/sys |
|---|---|---|---|
| min | 92.02 | 15.24 | 0.166× |
| **mean** | **126.59** | **17.26** | **0.136× (rust 7.33× faster)** |
| p50 | 123.27 | 16.35 | 0.133× |
| p90 | 148.51 | 19.78 | 0.133× |
| **p99** | **192.79** | **22.61** | **0.117× (rust 8.53× faster)** |
| max | 201.98 | 22.82 | 0.113× |

Rust startup is both lower **and far flatter** (15.2–22.8 ms across 50 samples). This is not bare interpreter boot against a like start: the oracle is a 14-scheduler SMP VM whose boot loses much more to the contended/oversubscribed host, while a `beam_rt` process is a small single-threaded binary that resolves its code path **natively from the filesystem**, with no `erl` spawn (§6.2). The whole-process figures are therefore the trustworthy cross-runtime startup comparison.

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `compute_loop_ms` p50 | 204.81 | 56.17 | 0.274× |
| `compute_ops_per_sec` | 4 885 889 | 17 162 012 | 3.51× (rust faster) |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 36 205 168 B (34.53 MiB) | **805 458 B (786.6 KiB)** | **44.95× lower** |
| peak RSS (`VmHWM`, whole workload) | 135 938 048 B (129.6 MiB) | 31 723 520 B (30.3 MiB) | 4.29× lower |

The loop is `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'` and runs under **both** runtimes.
**Caveat — read this loop as the native floor, not interpreter throughput.** It is dominated by the natively-served `lists:seq/2` + `lists:sum/1`; the execution-bound signal is §3.4–3.7.
`vm_memory_total_bytes` is a **separate** metric from the OS `peak_memory_bytes` (§6.5) and is never conflated with it: `beam_rt` serves `erlang:memory/0,1` through the `MemoryHook` seam, and the two runtimes are not reporting the same quantity (OTP's `total` includes allocator arenas and preloaded code; `beam_rt`'s is its much smaller in-process heap).

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

Every case printed the **same** stdout under both runtimes (report note: `add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5 base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, **mean** (ms); per-case peak RSS, **p50** (MiB, base 2):

| case | sys wall | rust wall | rust/sys wall | sys RSS | rust RSS | sys/rust RSS |
|---|---|---|---|---|---|---|
| `add` (fixture) | 138.01 | 18.74 | 0.136× | 66.15 | 15.14 | 4.37× |
| `fib` (fixture, n=20) | 135.93 | 18.06 | 0.133× | 66.25 | 15.15 | 4.37× |
| `pick` (fixture) | 139.99 | 17.27 | **0.123×** | 66.18 | 15.13 | 4.37× |
| `calendar_last_day` | 137.95 | 18.76 | 0.136× | 66.23 | 15.95 | 4.15× |
| `queue_from_size` | 138.71 | 22.40 | 0.162× | 66.10 | 17.89 | 3.70× |
| `gb_trees_size` | 139.30 | 18.13 | 0.130× | 66.47 | 15.89 | 4.18× |
| `base64_encode_size` | 130.45 | 19.44 | 0.149× | 66.24 | 17.17 | 3.86× |
| `ordsets_from_size` | 128.47 | 18.09 | 0.141× | 66.30 | 15.68 | 4.23× |
| `string_str_length` | 133.60 | 41.29 | **0.309×** | 66.17 | 26.25 | 2.52× |

Whole-workload peak RSS: sys **72 105 984 B (68.8 MiB)** / rust **27 660 288 B (26.4 MiB)** (2.61× lower).

**Read the wall column as *whole-process*, not *execution*.** Each case is a single call per process, so the wall time is dominated by process boot; six of the nine also call a *system* module. Per-case `rust` peak RSS **is** the like-for-like per-process memory comparison (one call per process, no loop accumulation): rust **15.1–26.3 MiB** vs sys **66.1–66.5 MiB** (2.52×–4.37× lower).
The `string_str_length` row is the family's **one-shot cold-start** case — the first entry pulls in a large Erlang module graph — and it is the one row where rust is above the ~17–22 ms boot floor (§3.9).

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source* (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system `erlc` and executed under both runtimes.
`ns/call` is the startup-cancelled marginal `(mean wall@N − mean wall@0) / N` (n = 10 samples each); `N` is per runtime so both marginals clear the timer noise. `wall_ms` is **not** comparable across runtimes (different `N`); `startup_ms` is.

| case (real function) | N sys / rust | sys ns/call | rust ns/call | rust/sys | startup p50 sys / rust (ms) | RSS p50 sys / rust (MiB) |
|---|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 1 500 000 / 20 000 | 371.84 | 19 606.73 | **52.7×** | 162.30 / 17.37 | 66.30 / 73.81 |
| `cowboy_bstr:capitalize_token/1` | 1 500 000 / 15 000 | 334.04 | 24 782.40 | **74.2×** | 136.09 / 16.18 | 66.20 / 76.35 |
| `emqx_base62:encode/1` | 3 000 000 / 25 000 | 222.49 | 10 283.30 | **46.2×** | 136.33 / 17.94 | 65.91 / 60.99 |
| `emqx_base62:decode/1` | 3 000 000 / 15 000 | 309.46 | 17 698.49 | **57.2×** | 140.83 / 17.35 | 66.43 / 62.96 |

All four cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout (`sys`: `bstr_to_lower=45000000 bstr_capitalize_token=45000000 emqx_base62_encode=48000000 emqx_base62_decode=33000000`; `rust`: `600000 / 450000 / 400000 / 165000` = `N × per_call`).

**This is the execution-bound result.** Startup is comparable while the *per-call* cost of a real foreign function is **~46×–74×** higher under `beam_rt`. The gap is a **real runtime cost, not a harness artifact**: the startup-cancelled marginal is stable and reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`. It is a `beam_rt` per-call host-context cost, not the engine's per-instruction dispatch (§3.8).
**`foreign` peak RSS is not a like-for-like memory comparison — see §6.7.**

### 3.5 `list_walk` — the per-element dispatch-throughput metric (n = 10/case)

A **BIF-free** body-recursive walk (`walk(N)` sums a 1000-element list `N` times; `build/2` makes the list once per process) runs under both runtimes; the reported figure is the startup-cancelled **ns per walked element**.

| runtime | `N` (walks) | elements walked | `ns_per_element` | `wall_ms` mean | `startup_ms` mean | peak RSS p50 |
|---|---|---|---|---|---|---|
| `sys` | 100 000 | 100 000 000 | **2.4238** | 380.30 | 137.92 | 66.16 MiB |
| `rust` | 1 000 | 1 000 000 | **158.8297** | 176.71 | 17.88 | 15.36 MiB |

`case_list_walk_elements` (the deterministic, contention-immune companion metric) is `1×10⁸` for sys and `1×10⁶` for rust; each run printed the deterministic constant `N × 500 500` (`rust`: `list_walk=N*500500=500500000`, `sys`: `…=50050000000`).
`ns/element` gives a **65.53×** rust/sys ratio, consistent with the §3.4 `foreign` per-call gap and the §3.6 `copy_bound` ratios: the same per-unit execution-throughput story, applied to pure dispatch.

### 3.6 `copy_bound` — the copy boundary: ETF, ETS and process send/receive (n = 10/case)

Three copy-boundary cases under both runtimes, each validating its own round-trip with a bound-variable pattern: an **ETF** `term_to_binary`/`binary_to_term` round trip, an **ETS** `ets:insert` + `ets:lookup` round trip, and a **two-process send/receive** copy. All three are startup-cancelled `ns/op`; the `ops` counter is deterministic.

| case | op | sys ns/op | rust ns/op | rust/sys | ops sys / rust |
|---|---|---|---|---|---|
| `etf_roundtrip` | `term_to_binary` / `binary_to_term` | 650.94 | 77 137.94 | **118.5×** | 1 500 000 / 5 000 |
| `ets_copy` | `ets:insert` + `ets:lookup` | 239.95 | 3 391.36 | **14.1×** | 2 000 000 / 100 000 |
| `send_recv` | two-process send/receive copy | 1 450.39 | 165 897.30 | **114.4×** | 500 000 / 2 000 |

All three ran under both runtimes (`3 ok, 0 unsupported`) with identical stdout (`sys`: `etf_roundtrip=1500000 ets_copy=2000000 send_recv=500000`; `rust`: `5000 / 100000 / 2000` = `N × 1`).
`ets_copy` is the cheapest boundary for both runtimes, and its rust/sys ratio (14.1×) is the narrowest in the whole suite — the other two (ETF encode/decode and the inter-process copy) are in the same ~114–119× band as the `foreign` per-call gap.
**`copy_bound` rust peak RSS is not like-for-like — the `ets_copy` row (N = 100 000) reaches 75.93 MiB vs sys 66.25 MiB (§6.7).**

### 3.7 `gc_bound` — allocate-and-abandon churn (n = 10/case)

`churn(N)` allocates and immediately abandons a 256-element list, a 256-byte binary and a 256-element tuple, `N` times; the reported figure is the startup-cancelled `ns/op`.

| runtime | `N` (churns) | `churn_ns_per_op` | `wall_ms` mean | `startup_ms` mean | peak RSS p50 |
|---|---|---|---|---|---|
| `sys` | 200 000 | **4 248.40** | 977.58 | 127.90 | 66.51 MiB |
| `rust` | 2 000 | **179 192.65** | 375.02 | 16.64 | 38.78 MiB |

`case_churn_ops` (deterministic) is `200 000` (sys) / `2 000` (rust); each run printed `churn=N*1` (matching its `N`).
The rust/sys ratio is **42.18×**. This row measures the allocate-and-abandon **wall time**; whether a runtime's collector actually runs during the loop is its own admission policy and is **not claimed here** (the report's own note says the same).

### 3.8 Reading the cross-runtime numbers

- **Startup / whole-process cost is the trustworthy cross-runtime comparison** (§3.1, §3.3): rust is 7.33× faster at `baseline` startup (mean) and 3.24×–8.11× faster per `slice` case, with 2.61× lower per-process peak RSS. A `beam_rt` process is small and starts in ~17–22 ms.
- **The startup-cancelled marginals are the per-unit execution-throughput signal** (§3.4–3.7), and they point the other way: rust costs **14.1×–118.5×** more per op/call/element. That residual is a `beam_rt` per-call runtime cost, not the engine's per-instruction dispatch (next bullet).
- **Only `slice` gives a like-for-like peak-RSS comparison** (§6.7). `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS grows with the per-runtime loop count `N`.
- The gap is a **`beam_rt` per-call host-context cost**, not `beam_engine` instruction dispatch (the dispatch arm is itself O(1) and allocation-free per instruction, and the baseline tier runs the common `gc_bif` arithmetic natively): the runtime-side costs behind it are `beam_rt` measurements owned by `../../crates/beam_rt/PERF.md` (the per-call dispatch section) and `../../crates/beam_rt/CONTEXT.md`, not `bench` output.
- **The `slice` whole-process column includes a one-shot cold start, and that cold start is bounded** (§3.9): the runtime path lowers a function on its first entry, so a process that enters a large Erlang module graph pays the first-entry compile cost rather than a whole-module compile.

### 3.9 One-shot cold start, and the **tier effect** (what the JIT default does and does not improve)

Every `slice` case is a single call in a **fresh** `beam_rt` OS process (§3.3), so that wall column carries the cost of entering a system module for the first time. That cost is bounded by construction: the **runtime path** stages a **lazy** native artifact — one sealed mapping per function, lowered on that function's first entry of a `(func, pc)` (`../../crates/beam_engine/src/jit/baseline/code.rs`) — so a fresh process pays only for the functions it actually enters. The **eager** whole-module layout (`JitBackend::compile` / `CodeCache::get_or_compile`) exists for the whole-module introspection surface and for the `jit_bench` harness; it is **not** the runtime path. The module-graph-heavy `string_str_length` row is the family's visible cold-start case (rust 41.29 ms mean vs the ~17–22 ms boot floor, §3.3).

**The tier effect is a Rust-vs-Rust comparison — it is NOT a comparison against the system ERTS.** The default `beam_rt` tier is the native baseline JIT; `BEAM_RT_TIER=interpreter` forces the interpreter. The delta between those two *Rust* columns is the **tier effect** (the JIT making the *Rust* runtime's own interpreter work cheaper, or occasionally dearer on short-lived processes), and it is measured and owned by `../../crates/beam_rt/PERF.md` §"`rust` default vs `BEAM_RT_TIER=interpreter` — the tier effect":

| comparison | workload | ratio | what it compares |
|---|---|---|---|
| **tier effect** (`crates/beam_rt/PERF.md`) | `foreign` | ~0.73×–0.87× | Rust default (baseline JIT) ÷ Rust `BEAM_RT_TIER=interpreter` |
| **tier effect** (`crates/beam_rt/PERF.md`) | `gc_bound` `churn` | ~0.54× | Rust default ÷ Rust interpreter |
| **rust-vs-system** (`crates/beam_rt/PERF.md`, rust/sys table) | `foreign` | **41×–64×** | Rust default ÷ system ERTS |
| **rust-vs-system** (this run, §3.4/§3.7) | `foreign` / `gc_bound` `churn` | **46.2×–74.2×** / **42.2×** | Rust default ÷ system ERTS |

> **Correction recorded here (do not propagate the misreading).** `CHANGE.md` (≈line 243–244) and the root `erts_rust/CONTEXT.md` (≈lines 148–150) state that the baseline-JIT default "took the `foreign` hot loop to **0.71×–0.84×** of the system ERTS (now faster) and `gc_bound/churn` to **0.49×**". That attribution is **wrong**: those ratios are the **tier effect** (Rust default ÷ Rust `BEAM_RT_TIER=interpreter`), **not** a Rust-vs-system-ERTS comparison. Against the *system* ERTS the default-tier rust figures are still far **dearer** — `foreign` **46×–74×** and `gc_bound/churn` **42×** in this run (§3.4/§3.7), matching `crates/beam_rt/PERF.md`'s own rust/sys table (41×–64× / ~30×). The two files named above are outside this node; this note is the correction, and the bench owner reports it upward.

**Caveat — `jit_bench` pins the hot path, not this path.** The engine's `jit_bench` micro-benchmark drives the **eager** whole-module artifact only, so its interpreter → baseline-JIT speed-ups (`../../crates/beam_engine/MEASURED.md` §"Measured (JIT tier micro-benchmark …)") describe the hot path with the compile amortised away, and say nothing about the lazy runtime path's per-entry compile cost.
The cited documents are **current-state** records — re-run their own commands for the live figures rather than quoting this summary. And the host-load caveat applies to them exactly as it does here (§6.1): absolute latencies move with load, so read the *ratio* (default ÷ interpreter), and interleave the two modes, never a single sample.

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS** on the target projects.
The Rust runtime cannot be scheduled on any of them (`Runtimes::SysOnly`, §5), so there is no Rust column.
All absolute values in this section are inflated by the varied host load (§1) and are **not comparable across runs**; §7 states each move explicitly.

### 4.1 `plb` — Programming-Language-Benchmarks (dotnet)

**Current state: `plb` runs.** The m9 measurement run recorded `failed(plb-build-failed)` (§7.1) — a **tool-resolution** defect, not a PLB bug, and it is now fixed.

**The defect (m9).** The shared tool policy (`src/workloads/tool.rs`) consulted **`PATH` first** and only then the version-ranked Nix-store scan, so a too-old `dotnet` on `PATH` **shadowed** the store's 9.0.318 SDK — the store scan was never consulted — and the `net9.0` build died instantly with `NETSDK1045`:

```
error NETSDK1045: The current .NET SDK does not support targeting .NET 9.0.
Either target .NET 8.0 or lower, or use a version of the .NET SDK that supports .NET 9.0.
```

**The fix (`src/workloads/tool.rs`).** A version-aware path, `tool::command_for_min(tool, nix_attr, min_version)`, resolves the tool by **version, not source order**: it enumerates **every** candidate (`PATH` → `$BENCH_TOOL_PATH` → the store scan), probes each one's `--version`, and selects the **greatest version that satisfies the caller's declared minimum** — with `PATH`/source order only as the tie-break. The requirement is caller-declared (`src/workloads/plb.rs`: `const MIN_DOTNET_SDK: &[u64] = &[9]`, because PLB's envs target `net9`); a candidate whose version cannot be probed never satisfies. When nothing satisfies, the resolver still returns the best available candidate and records an explicit `no <tool> >= X available; using the best available …` note — never a silent wrong pick. The chosen binary **and every rejection reason** are written into the result `notes`: `resolved dotnet 9.0.318 at <path> (<source>)` plus one `rejected dotnet 8.0.425 at <path> (<source>): requires >= 9` per failing candidate, so a failure names exactly what was tried and why it was rejected. (The `rebar3` store resolution and all other tools are unchanged; `command_for_min` is additive.)

**Smoke evidence — two runs, 2026-10-13 14:49–14:53 UTC, `--quick`, one workload at a time.**

| # | invocation | loadavg before → after | status | `wall_ms` | `test_time_ms` (n=10) | `peak_memory_bytes` |
|---|---|---|---|---|---|---|
| 1 | `bench run --quick --runtime sys --workloads plb` (default `PATH`) | `6.24 6.08 5.61` → `12.51 8.67 6.61` | `ok` | 73 591.25 | min 0 / mean 1 344.51 / p50 692.47 / p99 3 751.48 | 170 196 992 |
| 2 | same, with a real 8.0.425 store SDK forced to the **front of `PATH`** | `10.60 8.49 6.59` → `34.67 14.33 8.70` | `ok` | 72 613.24 | min 0 / mean 1 309.50 / p50 601.66 / p99 3 903.22 | 170 004 480 |

Exact command (run from the repo root; `--out-file` repointed at the tracked `results/` copy between runs):

```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb \
    --out-file erts_rust/tools/bench/out/plb-smoke.json --verbose
```

Provenance notes as recorded — run 1 (default `PATH`):

```
build stage ok (dotnet --task build took 25416 ms)
rejected dotnet 8.0.425 at /nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/bin/dotnet (nix-store): requires >= 9
rejected dotnet 8.0.425 at /nix/store/qybc3wpz3hz5daipbmnj7rgc73hn35j2-dotnet-sdk-wrapped-8.0.425/bin/dotnet (nix-store): requires >= 9
resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)
```

run 2 (real 8.0.425 store SDK forced to the front of `PATH`):

```
build stage ok (dotnet --task build took 19112 ms)
rejected dotnet 8.0.425 at /nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/bin/dotnet (PATH): requires >= 9
rejected dotnet 8.0.425 at /nix/store/9ql2v7xa762qgibgrq84prdz4sl212c7-dotnet-sdk-8.0.425/bin/dotnet (nix-store): requires >= 9
rejected dotnet 8.0.425 at /nix/store/qybc3wpz3hz5daipbmnj7rgc73hn35j2-dotnet-sdk-wrapped-8.0.425/bin/dotnet (nix-store): requires >= 9
resolved dotnet 9.0.318 at /nix/store/abbkzygy9aix7b6m9vqlm1xw766mlrf1-dotnet-sdk-9.0.318/bin/dotnet (nix-store)
```

**Run 2 is the decisive proof.** It reproduces the original defect condition exactly — a `real` 8.0.425 `dotnet` first on `PATH` (`which dotnet` → the 8.0.425 store bin) — and the resolver *rejects it* and falls through to the store's 9.0.318, so the build still succeeds. The two runs' `wall_ms` differ by ~1% for identical work, i.e. the figure is stable despite the loadavgs differing by ~3× (§ header caveat).

The smoke numbers are **`--quick` observations on a shared host**, not milestone figures: `test_time_ms` here is PLB's own per-test time across its 10 elixir kernels (n = 10, `min` 0 = a sub-millisecond kernel), `wall_ms` is the whole two-stage `--task build` + `--task bench` run (building the kernels' escripts dominates), and `peak_memory_bytes` is the driver's external `VmHWM` of the `.NET` tool. Read them as "PLB builds and runs on this host"; they do not belong in a cross-run comparison.

### 4.2 `khepri` — Khepri `rebar3 eunit` (GC / data-structure bound)

`rebar3` 3.27.1 is resolved from the Nix store by the driver's **store scan** — no `PATH` prefix and no env-var opt-in are needed (`rebar3` is not on `PATH`). It runs under **Erlang/OTP 28** (see the honesty note on `runtimes[]` version reporting).

| metric | value |
|---|---|
| `rebar3 eunit` wall | 77 755.91 ms |
| peak RSS | 360 857 600 B (344.1 MiB) |

### 4.3 `emqx` — EMQX umbrella build (`make emqx-enterprise-compile`)

`emqx` **does not run**: `skipped(emqx-requires-otp-28)`, with no number fabricated. The blocker is a genuine, mechanical toolchain constraint — **the checkout's exactly-declared pin vs the host's actual runtime**, verified first-hand:

* **The exact pin (required).** `28.5.0.7-2` — `/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`, and `/home/bill/Source/emqx/env.sh:3` → `export OTP_VSN=28.5.0.7-2`. The gate reads only these declared pins (via `required_otp`/`OtpPin` in `src/workloads/emqx.rs`); it never assumes checkout layout.
* **The exact available runtime.** `erl -noshell -eval 'io:format("~s ~s~n",[erlang:system_info(otp_release), erlang:system_info(version)]), halt().'` → `29 17.1`, resolved at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl` (the driver reports both the major and the erts version in the note).
* **The mechanism (why a major-version mismatch is fatal here).** The umbrella build aborts in the `:ezstd` dependency's NIF makefile, which regenerates its `env.mk` with the idiom `erl -noshell -s init stop -eval '…'`. On OTP 29 `init stop` halts **before** the `-eval` runs, so the idiom writes nothing — demonstrated directly on this host: `erl -noshell -s init stop -eval 'io:format("ENV_MK_WOULD_BE_WRITTEN~n"), halt().'` exits `0` with **no output at all** (on OTP 28 it prints the line). `mix compile` then fails with `No rule to make target '…/env.mk'`. EMQX documents the same constraint in its own `scripts/ensure-rebar3.sh`, whose `case ${OTP_VSN}` accepts only `25*`/`26*`/`27*`/`28*` and prints `Unsupported Erlang/OTP version $OTP_VSN` otherwise.
* **Why not "just override the pin".** There is no supported-version override that works: the only OTP-28 binding in the store (`erlang-28.5.0.7`) cannot be paired with the OTP-29-built **Elixir 1.20.4** / `mix` that the umbrella build's `mix` stage needs, and the pin lives in the **read-only** target checkout (the driver stages a copy, but editing the pinned version would be changing the project under test — not an honest measurement). Swapping the whole toolchain is out of this crate's scope. Nothing was changed to paper over it; the workload's own module docs say the same.

Recorded in `results/m9-bench-emqx.json` (m9) and re-confirmed by the 2026-10-13 14:51 UTC smoke run (`results/m9-bench-emqx-smoke.json`, loadavg `10.91 8.52 6.59` unchanged — the target skips immediately, before any build work).

### 4.4 `cowboy` — hello_world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s — `--full`)

`copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk git-fetches cowlib/ranch), starts the release and drives it with the in-process keep-alive loader.

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 40 503.48 ms |
| requests | 1 101 086 |
| **throughput** | **220 143.53 req/s** |
| errors | 0 |
| latency min / p50 / p90 | 0.02521 / 0.063911 / 0.102995 ms |
| **latency p99** | **0.276380 ms** |
| latency max | 3.592006 ms (n = 20 000) |
| peak RSS | 177 963 008 B (169.7 MiB) |

**Compared with the previous headline** (a prior full-mode run reported **117 813 req/s**, p50 **0.064 ms**, p99 **0.133 ms**): the p50 is essentially unchanged (0.0639 vs 0.064), while throughput is ~1.87× and p99 ~2.1× different. The in-process loader and the server share one contended machine and nothing about the release build or the loader changed, so this is **host-load variation, not a server change**. (Note the profile differs from the committed `m6-bench-cowboy.json` quick run — 8 conns × 2 s, 40 684.14 req/s, p50 0.1015, p99 1.628 — so that artifact is not directly comparable to this `--full` row. The prior full-mode headline of 117 813 req/s is the task-supplied reference; it is not present among the artifacts committed on this branch.)

### 4.5 `erlc_compiler` — `erlc` over the stdlib + Elixir corpus slice

| metric | value |
|---|---|
| wall | 6 094.92 ms |
| files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| **throughput** | **9.844 files/s** (**7 629.79 LOC/s**) |
| peak RSS | 135 974 912 B (129.7 MiB) |
| notes | `corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)` |

The `files_per_sec`/`loc_per_sec` rates are computed from the files/LOC `erlc` actually **produced** (the just-landed honesty fix; see the honesty audit), not from what was attempted — so a partial run cannot overstate them. Here all 60 attempted files produced a `.beam`, so produced = attempted and the note records both.

### 4.6 `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

Measures the **installed** `elixirc` (the in-repo checkout is unbuilt and read-only) over a staged `.ex` corpus (`--full` ⇒ up to 200 files).

| metric | value |
|---|---|
| wall | 48 696.61 ms |
| files attempted / `.beam` produced | 105 / **223** (97/105 compiled) |
| **throughput** | **4.579 files/s** (**2 299.22 LOC/s**) |
| compiler startup | 276.11 ms |
| peak RSS | 206 336 000 B (196.8 MiB) |
| notes | `batch compile failed; per-file fallback (version skew)`; `compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`; `223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)` |

**`files_attempted` (105) < `files_compiled` (223) is expected, not a bug:** the workload's own note explains that one Elixir source may define several modules, so the produced `.beam` count can exceed the source count. The throughput rate uses the produced count (223).

## 5. Workloads that did **not** run in the m9 measurement run — verified reasons

| workload | status | verified reason |
|---|---|---|
| `plb` | `failed(plb-build-failed)` **(m9 run; since fixed — see §4.1)** | The old tool policy resolved **`PATH` first**, and the `PATH` `dotnet` then was SDK **8.0.425** while the PLB elixir env targets `net9.0`, so `dotnet --task build` died with `NETSDK1045`. The store's 9.0.318 SDK was never reached (the version-ranked store scan was step 3, after `PATH`). No number was fabricated. The resolver now selects by version (`tool::command_for_min`), so `plb` runs (§4.1). |
| `emqx` | `skipped(emqx-requires-otp-28)` | EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`) while the host `erl` is OTP 29.1.1 / erts-17.1; the `:ezstd` `env.mk` make-target's `erl -noshell -s init stop -eval …` idiom produces nothing under OTP 29, so `mix compile` aborts. Not satisfiable without a full OTP-28 toolchain swap (§4.3). No number is fabricated. |
| every `SysOnly` workload scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/bootstrap an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. (Not exercised as a row in m9 — the m9 sys-only runs used `--runtime sys` — but it is the harness behaviour.) |

## 6. Reproducibility, stability and known measurement caveats

### 6.1 The host is shared and loaded, and the two runtimes are measured in different windows
The 1-min `/proc/loadavg` across the m9 session ran from **4.11 to 28.95** against a 14-core host (§1, `results/m9-load-record.md`); the primary full runs happened at the quiet end (11.01 → 6.02 and 4.68 → 4.43) but earlier quick passes were higher. The runner is **runtime-major** (`sys` rows first, then `rust` rows), so under a drifting load the two sides see different conditions. Loadavg is host-wide and does **not** move monotonically with the measured figures (§7), so prefer ratios, prefer the startup-cancelled rows, and always record `/proc/loadavg` alongside an absolute figure. The `m4` primary run (load 4.77 → 11.59) remains the historical near-quiet reference.

### 6.2 `beam_rt` resolves its code path natively from the filesystem
The runtime locates its system `ebin` directories (stdlib, kernel, erts) with a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`: `$ERTS_STDLIB_DIR` when set, else the standard OTP installation layout under the `erl` on `$PATH` — *resolved to a path, never executed* — then this executable's ancestors, `$ERL_TOP`, and standard prefixes). The system `erl` is only *run* as a last resort, for an installation that layout cannot see. That is why rust startup is ~17–22 ms per process rather than the ~230–460 ms of the `m5` run. Setting `$ERTS_STDLIB_DIR` still short-circuits the probe; `bench` never sets it (the harness is a pure driver/observer and never mutates the runtime's environment beyond argv). The per-element/per-call metrics cancel whatever startup cost remains by construction.

### 6.3 `elixir_compiler` measures the installed `elixirc` 1.20.4
against the unbuilt, read-only 1.21.0-dev checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the artifact.

### 6.4 `cowboy` re-fetches cowlib/ranch in a fresh scratch copy on every run (network)
which is why its `build_ms` (~40.5 s) dominates the workload.

### 6.5 Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`)
Erlang's own `erlang:memory(total)` is a **separate** metric (§3.2) and is never conflated with it.

### 6.6 `foreign` / `list_walk` / `copy_bound` / `gc_bound` `wall_ms` are not comparable across runtimes
Each side runs a different `N`. Compare the startup-cancelled marginals (`ns_per_call`, `ns_per_element`, `ns_per_op`) and `startup_ms`.

### 6.7 The rust peak RSS of `foreign` / `list_walk` / `copy_bound` / `gc_bound` still grows with the loop count `N` — it is **not** a like-for-like memory comparison
`beam_rt` has a process-heap collector (`crates/beam_rt/src/runtime/gc.rs`, driven from `context/gc.rs` at the scheduled-slice heap-admission point), but it does **not** bound peak RSS on the paths the harness drives. A controlled probe (a fresh 1000-element list built and walked in a tail loop, identical under `beam_rt run … <N>` and `erl -noshell -eval`) reads:

| N (iterations) | rust peak `VmHWM` | sys peak `VmHWM` |
|---|---|---|
| 1 000 | 29 MiB | 61 MiB |
| 5 000 | 77 MiB | 60 MiB |
| 20 000 | 227 MiB | 60 MiB |
| 50 000 | 484 MiB | 61 MiB |

`beam_rt` peak RSS tracks the allocation roughly linearly; the oracle's is bounded.
**Consequence:** the m9 rust `case_<name>_peak_rss_bytes` rows run at large `N` and are **not** like-for-like — e.g. `foreign` rust p50 is 61.0–76.4 MiB vs sys 65.9–66.4 MiB, and `copy_bound`'s `ets_copy` row reaches 75.93 MiB (N = 100 000) vs sys 66.25 MiB. Use `slice` (§3.3, one call per process, `N`=1) as the comparable per-process metric.

## 7. Deltas vs the previous run (`m6`, and `m5`/`m4`)

Every figure below is quoted from the committed artifacts: `m9-*` (2026-10-13, full-pass load 11.01 → 6.02 / 4.68 → 4.43), `m6-*` (2026-10-11, cross-runtime load ≈414–440), `m5-*` (2026-10-09, load ≈90–153) and `m4-*` (2026-10-08, load 4.77 → 11.59). **The runs sit at very different — and non-monotonic — host loads.**

**Cross-run comparability.** Only the **load-invariant** rows are safe to compare across runs: the startup-cancelled marginals and, above all, the **deterministic counters** (`*_ops`, `*_elements`). The absolute wall / startup / throughput rows are **not** cross-run comparable — they moved in *both* directions between runs (several sys-only throughputs rose despite a higher recorded loadavg), which is why `/proc/loadavg` is treated as a weak proxy throughout this document.

### 7.1 m9 vs the m6 regression baseline — **not a regression**

**Headline: no figure is a regression.** Every `rust` **per-unit marginal** and the `baseline` startup/churn rows are **equal to or better** than the m6 record, and the `sys` (oracle) side is unchanged within load.

Cross-runtime (`xrun`), m6 → m9:

| metric (mean) | runtime | m6 | m9 | move |
|---|---|---|---|---|
| `baseline` `startup_ms` | rust | 17.497 | **17.265** | equal/better |
| `baseline` `startup_ms` | sys | 157.093 | **126.588** | lower, under a far lower recorded load |
| `baseline` `vm_memory_total_bytes` | rust | 805 458 | **805 458** | unchanged |
| `foreign` `ns_per_call` | rust | 27 184.74–41 393.44 | **10 283.30–24 782.40** | **improved** (every case) |
| `foreign` `ns_per_call` | sys | 216.54–423.66 | 222.49–371.84 | unchanged within load |
| `list_walk` `ns_per_element` | rust | 340.502 | **158.830** | **improved** |
| `list_walk` `ns_per_element` | sys | 3.1312 | 2.4238 | unchanged within load |
| `copy_bound` `ns_per_op` | rust | etf 121 953.98 / ets 4 281.73 / send 194 813.33 | **etf 77 137.94 / ets 3 391.36 / send 165 897.30** | **improved** (all three) |
| `copy_bound` `ns_per_op` | sys | 647.48 / 244.59 / 1 452.94 | 650.94 / 239.95 / 1 450.39 | unchanged within load |
| `gc_bound` `case_churn_ns_per_op` | rust | 258 354.39 | **179 192.65** | **improved** |
| `gc_bound` `case_churn_ns_per_op` | sys | 4 643.26 | 4 248.40 | unchanged within load |

(`slice` whole-process walls moved in **both** directions by a few ms — `add` 17.95→18.74, `fib` 17.92→18.06, `string_str_length` 36.90→41.29 up; `pick`, `gb_trees_size`, `ordsets_from_size`, `queue_from_size`, `base64_encode_size`, `calendar_last_day` down. Those rows are startup-bound single-call samples whose oracle side also fell (m6's `fib` sys mean was an outlier 228.73 vs m9 135.93), i.e. host-load/`sys`-scheduling noise, not a rust regression. The rust boot floor is unchanged.)

Sys-only external targets (`m9-bench-external.json`, `--full`) vs the m6 quick artifacts — **not directly comparable** (profiles differ: m6 quick vs m9 full), so read these as current-state only:

| workload | metric | m6 (quick) | m9 (full) |
|---|---|---|---|
| `khepri` | `rebar3 eunit` wall | 101 386.88 ms | 77 755.91 ms |
| `cowboy` | `requests_per_sec` | 40 684.14 (8 conns × 2 s) | 220 143.53 (16 conns × 5 s) |
| `erlc_compiler` | `files_per_sec` | 6.236 (60 files) | 9.844 (60 files) |
| `elixir_compiler` | `files_per_sec` | 1.796 (25 attempted / 39 produced) | 4.579 (105 attempted / 223 produced) |
| `plb` | status | `ok` (.NET 9 store SDK present) | `failed(plb-build-failed)` (.NET 8.0.425 on `PATH` shadowed it) — **now fixed; `plb` runs** (§4.1) |

### 7.2 Real, code- and environment-attributable changes since m6

* **The rust per-unit marginals improved across the board** (the table in §7.1): `foreign` per-call, `list_walk` per-element, `copy_bound` per-op and `gc_bound` per-op are all lower than m6 on the rust side, while the oracle side is unchanged. This is the current-state execution-throughput story; the direction and the `~14×–119×` band are the same.
* **`plb` flipped from `ok` to `failed`** — and it was a **tool-resolution** change, not a PLB regression. m6 realized `nixpkgs#dotnet-sdk_9` in the store and the store scan resolved 9.0.318; in the m9 environment a `dotnet` **on `PATH`** (`8.0.425`) won step 1 of the shared policy, so the store's 9.x SDK was never reached and the `net9.0` build aborted with `NETSDK1045` — recorded honestly as `failed(plb-build-failed)` (§4.1, §5). **Since fixed:** the resolver now selects by version (`tool::command_for_min`), so `plb` runs even with an 8.0.425 `PATH` `dotnet` present (2026-10-13 smoke, §4.1).
* **`erlc_compiler`'s throughput rates now divide the *produced* `.beam`/LOC counts** (a just-landed honesty fix), not the attempted counts. With 60/60 produced in both runs the m9 numbers are produced-based by construction; a partial run can no longer overstate throughput next to `files_compiled`.

### 7.3 Load-attributable / not comparable

* **`baseline` startup, `sys` side.** m6 mean 157.09 ms → m9 mean 126.59 ms — *lower*, at a much lower recorded loadavg (§1); host variation, not a code change.
* **Cowboy throughput.** m9 220 143.53 req/s (p99 0.2764) against the prior full-mode headline 117 813 req/s (p99 0.133) — p50 unchanged; the move is the shared host's load, per §4.4.
* **Compiler throughput / `khepri` wall.** Both ran the unmodified system toolchain; the m9 rates are higher and the corpus/profile differ (full vs quick), so the move is not code-attributable.

### 7.4 Unchanged

* **`baseline` `vm_memory_total_bytes` (rust)** — m6 805 458 B → m9 805 458 B (the two runtimes still report different quantities, §3.2).
* **`foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS still scales with `N`** (§6.7).
* **The per-unit execution-throughput gap is the stable cross-runtime story.** `m4` (quiet) `foreign` 53.8×–72.0×; `m5` 53.1×–96.0×; `m6` 74.9×–131.4×; `m9` **46.2×–74.2×**. A different host load, the same per-unit cost. `list_walk` tells the same story: `m5` rust 561.67 ns/element vs sys 8.43 (66.6×) → `m6` rust 340.50 vs sys 3.13 (108.7×) → `m9` rust 158.83 vs sys 2.42 (65.5×).

## Honesty audit — measurement-integrity notes

No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always `failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`, `src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`/non-doc `unreachable!`, no `#[ignore]`.

- **`foreign` classifies a staging failure honestly** (`src/workloads/foreign.rs`: `StageError` + `stage_outcome`): a checkout whose foreign `.erl` sources are genuinely absent ⇒ `skipped(sources-missing)`, while a failure to compile the sources we *did* stage (the embedded driver or a present foreign source) ⇒ `failed(module-compile-failed)` — matching the workloads whose sources are ours (`list_walk`/`copy_bound`/`gc_bound`). `slice` treats a failed fixture rebuild as `skipped(fixture-missing)`.
- **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` are computed from the *produced* counts** (`rates_per_second` over `CorpusRun::produced` / `produced_loc`, the latter from `compiled_sources`, which matches each newly written `.beam` back to its source by file stem), so a partial `erlc` run cannot overstate throughput next to `files_compiled`. The attempted file/LOC totals are still reported, but only in a note. `elixir_compiler` likewise uses the produced count.
- **A version-constrained tool is resolved by version, and the choice is auditable.** `plb` calls `tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` (min `9`), which probes every candidate's `--version` and selects the greatest satisfying one (§4.1). The resolved binary + version + source and every rejected candidate's reason are recorded in the row's `notes` — in **both** the success and the build-failure branch, so a `failed` row names the exact SDK it used. If no candidate satisfies, the resolver still returns the best available one and records a `no <tool> >= X …` note — it never silently picks a too-old tool, and it never fabricates a number. `khepri`'s `runtimes[]` version describes the driver's oracle, not necessarily the Erlang the store `rebar3` escript chose (§4.2).
- `proc.rs`'s drain-thread join (`h.join().unwrap_or_default()`) is infallible **by construction** — the drain threads run only `read_to_end` + a lossy UTF-8 conversion, neither of which can panic — so the `unwrap_or_default` masks nothing real; the invariant is documented at the call site.

`baseline.rs`'s docs correctly state that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its `MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`; `is_capability_gap` survives only as a fallback classifier.

The `foreign`/`list_walk`/`copy_bound`/`gc_bound` startup-cancelled methodology is implemented exactly as this document states (startup-cancelled `(mean wall@N − mean wall@0) / N`), and the registered-BIF dispatch cost — **≈ 79–96 ns/call** for the non-inlinable `call_ext` round trip (`../../crates/beam_rt/PERF.md`, *Registered-BIF (non-inlinable) per-call*) and **≈ 76 ns/iter** for `pb:tr/2`'s `trunc/1` (`../../crates/beam_rt/TRACE_AND_PERF.md`, *Dispatch cost (registered-BIF path)*) — is **not** produced here; it is a `beam_rt` measurement.

**Reproducibility note (PLB / tool resolution).** `plb`'s outcome is *tool- and environment* state, not repository state. The resolver now picks the **greatest `dotnet` ≥ 9.0** across `PATH` + `$BENCH_TOOL_PATH` + the Nix store, so `plb` runs wherever any 9.x+ SDK is reachable — even with an older `dotnet` first on `PATH` (the run-2 smoke of §4.1 proves exactly that). It is `skipped(dotnet-absent)` only if no `dotnet` resolves at all, and if a `dotnet` resolves but none is ≥ 9 it still returns the best available one with an explicit `requires >= 9` note, so the resulting `NETSDK1045` failure names the SDK. `khepri` needs only the `rebar3` store entry and runs as-is.

## Archived artifacts

| file | contents |
|---|---|
| `results/m9-bench-xrun.json` | **current cross-runtime run**: `--full --runtime all`, the six `Runtimes::Both` workloads × both runtimes, 12/12 rows `ok` (full pass, 1-min load 11.01 → 6.02, wall 2m44s) — the source of §3 |
| `results/m9-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside `m9-bench-xrun.json` (69 numeric rows; the tool diffs the **mean**) |
| `results/m9-bench-external.json` | the external sys-only targets in one `--full` report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri` (§4.2, §4.4–§4.6; load 4.68 → 4.43, wall 3m00s) |
| `results/m9-bench-plb.json` | `plb` during the m9 measurement run → `failed(plb-build-failed)` (`NETSDK1045`, `PATH` `dotnet` 8.0.425; §4.1) — the pre-fix observation |
| `results/m9-bench-emqx.json` | EMQX → `skipped(emqx-requires-otp-28)` (§4.3) |
| `results/m9-bench-plb-smoke.json` | **post-fix smoke (2026-10-13 14:49 UTC)**: `plb` → `ok`, `wall_ms` 73 591.25, store 9.0.318 resolved from `PATH` (§4.1) |
| `results/m9-bench-plb-smoke-path8.json` | **post-fix smoke, defect condition reproduced (14:51 UTC)**: a real 8.0.425 `dotnet` forced first on `PATH` → still `ok`, store 9.0.318 resolved (§4.1) — the decisive proof |
| `results/m9-bench-emqx-smoke.json` | **post-fix smoke (14:51 UTC)**: EMQX re-confirmed → `skipped(emqx-requires-otp-28)` with the exact pin/mechanism note (§4.3) |
| `results/m9-load-record.md` | the m9 pass: exact commands, the `/proc/loadavg` before/after each run, and the note on which passes were quick vs full |
| `results/m9-tool-targets-notes.md` | the factual per-target status/blocker notes and the environment findings (`dotnet` on `PATH`, `clang` present, `rebar3` store-resolved) |
| `results/m6-*` | the **previous** run (2026-10-11, cross-runtime load ≈414–440): the focused cross-runtime report, the per-target runs, the cross-runtime diff, and the `m6-load-record.md`/`m6-tool-targets-notes.md` records — **regression baseline for §7** |
| `results/m5-*` | the run before that (2026-10-09, load ≈90–153): primary full run, focused `xrun`, PLB runs, cross-runtime diff |
| `results/m4-*` | the run before that (2026-10-08, load 4.77 → 11.59): primary full run, the three xruns, the compiler/cowboy/khepri runs, the PLB attempts, and the `m4-compare-*.txt` diffs — the near-quiet reference |

The archived copies are the harness output except for two mechanical normalisations that strip environment-specific absolute paths: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` is replaced with `$TMPDIR/`. **No metric value is altered.**
