# Rust-ERTS benchmark results — current measurement run (m11)

Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured, never modelled**.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-14 05:47–06:57 UTC** (= 2026-10-14 13:47–14:57 +0800) at commit **`ab6f1b5e58835cbf5fcb75257679532c46090a74`** (`ab6f1b5e58`, "bench(copy_bound): add send_recv_plain delivery-only attribution case"), with the driver and `beam_rt` both built from this tree (`--release -p beam_rt -p bench`).
Raw harness JSON is committed under `results/` as `m11-*`; the previous runs are kept as `m10-*` (2026-10-13), `m9-*` (2026-10-13), `m6-*` (2026-10-11), `m5-*` (2026-10-09) and `m4-*` (2026-10-08) and are the regression baselines for the delta section (§7).

> **Naming both sides of every ratio (read this before quoting a number).** `sys` = the **system ERTS** (OTP 29.1.1, the oracle `erl`, a 14-scheduler SMP VM). `rust` = **`beam_rt`**, the Rust runtime, at its **default execution tier** (the native baseline JIT). A third, distinct column exists — **`rust` with `BEAM_RT_TIER=interpreter`** — and it is a **Rust-vs-Rust tier effect, NOT a system-ERTS comparison** (§3.9, §7). The m9 record once published a `foreign` figure of **0.71×–0.84×** as "of the system ERTS" when it was actually that Rust-vs-Rust tier effect; against the system ERTS the same loop was **46.2×–74.2×** dearer. **Every ratio in this document names both sides.**
> **The target applications are NOT run by the Rust runtime.** `cowboy`, `khepri`, `plb`, `erlc_compiler`, `elixir_compiler` and `emqx` are executed by their **own system-ERTS / system-toolchain** (`erlc`, `elixirc`, the Cowboy release on the oracle `erl`, `rebar3`'s Erlang, the `.NET` tool). They are **reference** figures for the sys-only surfaces (§4), never Rust-runtime results.

> **This is a shared, usually-loaded host** (§1, §6.1): absolute latencies/throughputs move several-fold with load, so **ratios are the signal** and a single contended sample is never a constant. The m11 passes ran at 1-min loads `6.04 → 2.98` (cross-runtime), `1.81 → 2.59` (interpreter-tier control, `--quick`) and `1.36 → 2.36` (external targets); the cross-runtime pass **fell** during the run, so its later (`rust`) side ran under a *lighter* load.

## Verdict

**(a) / (b) Which targets run, which skip.** Five of the six external target projects run; one (`emqx`) skips with a verified toolchain reason (never a fabricated number). All five running rows are executed by their **own system toolchain**, not by the Rust runtime.

| target project | runs? | executed by | result / verified reason |
|---|---|---|---|
| `khepri` (tree DB) | **runs** | `rebar3`'s Erlang (OTP 28 escript; §4.2) | `ok` — `rebar3 eunit` wall 83 402.90 ms |
| `cowboy` (HTTP server) | **runs** | the oracle `erl` release | `ok` — 190 626.72 req/s, p50 0.0787 ms / p99 0.2234 ms |
| `erlc_compiler` (Erlang compiler) | **runs** | system `erlc` (OTP 29) | `ok` — 7.589 files/s over the stdlib+Elixir corpus |
| `elixir_compiler` (Elixir compiler) | **runs** | installed `elixirc` 1.20.4 | `ok` — 4.678 files/s over 105 sources / 223 `.beam` |
| `plb` (Benchmarks Game kernels, .NET) | **runs** | the `.NET` tool (`dotnet` 9.0.318) | `ok` — wall 68 318.69 ms; `test_time_ms` p50 559.98 / p99 3 355.48 (n=10) |
| `emqx` (MQTT broker) | **skips** | — | `skipped(emqx-requires-otp-28)` — EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`), the host `erl` is OTP 29.1.1 / erts-17.1; not satisfiable without a full OTP-28 toolchain swap (§4.3, §5) |

The six workloads the Rust runtime itself can host (`Runtimes::Both`) — `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound` — all ran: **12/12 rows `ok`** (0 skipped, 0 unsupported, 0 failed). Every other workload is `SysOnly` and would report `skipped(runtime-unsupported)` under `rust` (§5).

**(c) Ratio to the system-ERTS oracle, per cross-runtime workload.** `rust/sys` is the **Rust default tier** cost as a multiple of the **system-ERTS** cost (`<1` ⇒ Rust cheaper, `>1` ⇒ Rust dearer). Values are the m11 `xrun` (§3, `--full`, 1-min load 6.04 → 2.98).

| workload | surface | headline metric | sys (system ERTS) | rust (`beam_rt`, default tier) | rust/sys | reading |
|---|---|---|---|---|---|---|
| `baseline` | startup / CLI | `startup_ms` (mean, whole process) | 112.8126 ms | 15.2122 ms | **0.135×** | Rust **7.42× faster** per process |
| `slice` (9 programs) | compute | `case_*_wall_ms` (mean, per case) | 118.79–122.52 ms | 15.35–37.85 ms | **0.127×–0.311×** | Rust **3.21×–7.86× faster** |
| `foreign` (Cowboy/EMQX source) | compute / `.beam` | `case_*_ns_per_call` | 210.23–344.97 ns | 19 306.39–77 167.32 ns | **60.2×–367.1×** | Rust **60.2×–367.1× dearer** per call |
| `list_walk` | dispatch | `case_list_walk_ns_per_element` | 2.4194 ns | 45.9731 ns | **19.0×** | Rust **19.0× dearer** per element |
| `copy_bound` (ETF / ETS / send-recv, **4 cases**) | Msg / copy | `case_*_ns_per_op` | 236.93–1 453.37 ns | 2 435.07–21 522.24 ns | **10.3×–30.7×** | Rust **10.3×–30.7× dearer** per op |
| `gc_bound` (allocation churn) | GC | `case_churn_ns_per_op` | 4 270.13 ns | 128 524.45 ns | **30.1×** | Rust **30.1× dearer** per op |

- **The two directions are both real and answer different questions.** Rust's *whole-process cost* is far lower (a `beam_rt` process starts in ~15.2 ms vs the oracle's ~113 ms), while the *per-unit execution cost* of Erlang code under `beam_rt` is far higher (**10.3×–367.1×**, §3.8). Startup / whole-process cost is the trustworthy whole-process comparison; the startup-cancelled marginals are the per-unit execution-throughput signal.
- **The stable per-unit band narrowed again since m10** — the `copy_bound` per-op band fell **13.6×–54.7× → 10.3×–30.7×**, `list_walk` **22.1× → 19.0×**, `gc_bound churn` **33.7× → 30.1×**; `foreign` moved the *other* way (**52.4×–78.7× → 60.2×–367.1×**, §8 item 1) and its `ns_per_call` row is **not a clean per-call cost at these `N`** (see §3.4 / §8 item 1).
- **Peak RSS is a like-for-like cross-runtime comparison only for `slice`** (one call per process). For `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS still grows with the per-runtime loop count `N` and is **not** comparable (§6.7) — though `list_walk` and `gc_bound` sit **below** the oracle (§7).
- **Load caveat (read before quoting any absolute number).** The m11 passes ran at 1-min `/proc/loadavg` `6.04 → 2.98` (cross-runtime), `1.81 → 2.59` (interpreter-tier control, `--quick`) and `1.36 → 2.36` (external targets); the host is shared and absolute latencies are **not comparable across runs**. Prefer the ratios above, the startup-cancelled marginals and the deterministic counters.

## Coverage — workload ↔ rewrite surface

The driver exercises the messaging/copy, ETF, ETS and GC surfaces named in the top-level `ERTS_RUST_REWRITE_TEST_PROJECTS.md` coverage matrix, **under both runtimes**, via the workloads `copy_bound` / `gc_bound`; the I/O, compute and compiler surfaces are covered too (the I/O and compiler ones only on the system runtime).

| workload | category | rewrite surface exercised (TEST_PROJECTS matrix) | runtime |
|---|---|---|---|
| `baseline` | startup | startup / p99 tail; CLI (A.6); boot (A.7) | **both** |
| `slice` | compute | JIT/compute (calls real `calendar`/`queue`/`gb_trees`/`base64`/`ordsets`/`string` code); boot (A.7) | **both** |
| `foreign` | compute | JIT/compute on **real Cowboy + EMQX source**, compiled by `erlc` (`.beam` A.2) | **both** |
| `list_walk` | compute | JIT/compute — a BIF-free body-recursive walk (per-element dispatch) | **both** |
| `copy_bound` | messaging | **Msg/copy + Distrib/ETF + ETS** — `term_to_binary`/`binary_to_term`, `ets:insert`+`ets:lookup`, two-process send/receive (with and without the ETF leg in the receiver) | **both** |
| `gc_bound` | gc | **GC** — allocate-and-abandon churn | **both** |
| `cowboy` | io | I/O — Ranch/`epoll` socket path, `cowlib` HTTP parse, per-request process spawn | system-only |
| `khepri` | gc | ETS/`ordered_set`, GC pressure, disk I/O | system-only |
| `plb` | compute | JIT/compute — Benchmarks-Game kernels (Erlang/Elixir) | system-only |
| `erlc_compiler` | compiler | compiler throughput (`erl_parse`/`erl_lint`/`beam_ssa_*`), file I/O | system-only |
| `elixir_compiler` | compiler | compiler throughput (Elixir toolchain) | system-only |
| `emqx` | messaging | Msg/copy, Distrib/ETF, NIF ABI (`ssl`/`crypto`) | system-only (**skipped**, §4.3) |

> **Boot / contract A.7 status at this commit — NOT complete.** `baseline` and `slice` are both registered against A.7 (process boot / `-boot`), but **no boot completion is demonstrated by this driver**. At the measured commit, `erts_rust/target/release/beam_rt -boot bootstrap/bin/start` exits **3** with the single stderr line `beam_rt: unsupported: opcode: run: process blocked (no message available)` — the real `bootstrap/bin/start.boot` stops at **directive 18 of 22** (the driver itself only exercises the plain per-process `-eval`/`run` startup path, which does complete). Recorded as fact; see §6.8.

**Surfaces still uncovered by this driver:** the **NIF C ABI** (A.3) and **driver C ABI** (A.4), the **hot-upgrade** surface, and the **live distribution wire** — `copy_bound`'s `etf_roundtrip` case covers the **ETF codec** (A.5) as a single-process round trip, not a two-node handshake / `DOP_*` loopback. Those surfaces are covered by the differential gate (`tools/differential`, contracts A.3/A.4), not by `bench`.

## Host contention (read before quoting any absolute number)

> The benchmark host is a **shared 14-core machine** and was loaded throughout m11.
> The primary (**full**) passes were taken at `/proc/loadavg` 1-min **6.04 → 2.98** (cross-runtime), **1.36 → 2.36** (external targets) and **1.81 → 2.59** (interpreter-tier control, `--quick`); the PLB pass ran `2.36 → 1.97`, EMQX `1.97 → 1.97` (`results/m11-load-record.md` records the exact sequence, and the session 1-min value ranged **1.12 → 7.18**).
> Every *absolute* latency / wall / throughput below is still inflated by CPU contention and is **not comparable across runs**. Prefer **ratios**, the **startup-cancelled per-op / per-call / per-element marginals** (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counters** (`*_ops`, `*_elements`).
> `/proc/loadavg` is host-wide and a weak proxy — it does **not** move monotonically with the measured figures (§7) — so record it, but never infer a slowdown from it alone.
> The load was recorded **before and after every run** (§1/`results/m11-load-record.md`) — mandatory here.

## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (UTC) | 2026-10-14 05:47–06:57 UTC (= 2026-10-14 13:47–14:57 +0800) |
| Commit measured | `ab6f1b5e58835cbf5fcb75257679532c46090a74` |
| Host / kernel | Linux 7.2.9 (`x86_64`), NixOS 26.11 (Zokor, build `26.11.20261003.a7868a7`) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 65 317 432 kB (62.29 GiB) |
| Host load (1-min `/proc/loadavg`) | full cross-runtime `xrun` **6.04 → 2.98**; full external targets **1.36 → 2.36**; interpreter-tier control (**`--quick`**) **1.81 → 2.59**; matched default-tier control (**`--quick`**) **2.38 → 2.16**; PLB **2.36 → 1.97**, EMQX **1.97 → 1.97** (per-run table in `results/m11-load-record.md`) |
| Toolchain | `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl` at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`, **OTP 29.1.1 / erts-17.1** (`otp_release` = `"29"`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` / `mix` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, resolved from the Nix store by the driver's **store scan** (**not** on `PATH`); its shebang runs it under **Erlang/OTP 28** — see §4.2 |
| `dotnet` | **9.0.318, on `PATH`** (`/run/current-system/sw/bin/dotnet`). The store also holds two 8.0.425 SDKs; the version-aware resolver rejects them for `plb` (declared min `9`) and records each rejection (§4.1) |
| `clang` | **21.1.8**, on `PATH` |
| `$BENCH_TRY_NIX` / `$BENCH_TOOL_PATH` | **unset** for the m11 runs — the store scan resolves `rebar3` locally (§4.2) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` |
| `/usr/bin/time` | **absent** — the driver's own timers and `/proc/<pid>` `VmHWM` polling are used |

Both runtimes resolve and pass the harness self-check (`supported: true`).

Per-run `/proc/loadavg` (1-min value first; the full sequence, with the exact commands, is in `results/m11-load-record.md`):

| run | before | after |
|---|---|---|
| **FULL cross-runtime pass** (`m11-bench-xrun.json`, wall 2m54s) | `6.04` | `2.98` |
| (unarchived interpreter diagnostics; §3.1) | `3.53` | `2.07` |
| **FULL external-target pass** (`m11-bench-external.json`, wall 3m11s) | `1.36` | `2.36` |
| PLB quick pass (`m11-bench-plb.json`, wall 1m33s) | `2.36` | `1.97` |
| EMQX quick pass (`m11-bench-emqx.json`, skips immediately, wall 0.7s) | `1.97` | `1.97` |
| interpreter-tier control, **`--quick`** (`m11-bench-xrun-interp.json`, rust-only, wall 12m27s) | `1.81` | `2.59` |
| matched default-tier control, `--quick` (`m11-bench-xrun-quick.json`, rust-only, wall 17s) | `2.38` | `2.16` |

## 2. Exact commands

Build the driver and the runtime:
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
```
Probe status:
```sh
erts_rust/target/release/bench list --json       # all 12 workloads probe_ready: true, 0 skip reasons
```
The **focused cross-runtime run** — all six `Runtimes::Both` workloads, `--full`, both runtimes → `results/m11-bench-xrun.json` (wall 2m54s):
```sh
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m11-bench-xrun.json --verbose
```
The **tier-effect control** — the same workloads, `rust` only, interpreter pinned → `results/m11-bench-xrun-interp.json` (wall 12m27s). **This run is `--quick`, not `--full`**: at this commit the interpreter tier is pathologically slow (and super-linear in `N`) on the `foreign` cross-module cases (§3.9), so a `--full` pass would need ≈50 min. A **supplementary matched-form default-tier `--quick` control** → `results/m11-bench-xrun-quick.json` (wall 17s) exists so the tier table can be read off two `--quick` runs instead of pairing a `--quick` run against the `--full` `m11-bench-xrun.json`:
```sh
BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m11-bench-xrun-interp.json --verbose
erts_rust/target/release/bench run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/out/m11-bench-xrun-quick.json
```
Cross-runtime diff, the as-specified tier diff and the **matched-form** tier diff (archived as text artifacts; the tool diffs the **mean**):
```sh
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m11-bench-xrun.json \
    > erts_rust/tools/bench/results/m11-compare-cross-runtime.txt
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m11-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m11-bench-xrun.json \
    > erts_rust/tools/bench/results/m11-compare-tier-effect.txt       # a=quick, b=full (form mismatch)
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m11-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m11-bench-xrun-quick.json \
    > erts_rust/tools/bench/results/m11-compare-tier-effect-quick.txt  # matched-form quick<->quick
```
The **full sys-only external-target run** (one report, four workloads; wall 3m11s) and the two remaining single-target runs:
```sh
erts_rust/target/release/bench run --full  --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/out/m11-bench-external.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/out/m11-bench-plb.json  --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/out/m11-bench-emqx.json --verbose
```
The runtime's boot status at this commit (a factual probe, not a workload):
```sh
erts_rust/target/release/beam_rt -boot bootstrap/bin/start     # exit 3; §6.8
```
Crate tests: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` (**251 passed / 0 failed** at this commit: 243 lib unit + 4 integration + 1 oracle smoke + 3 doc).

Profile semantics: `--full` ⇒ `baseline` 50 startup samples / 5 compute samples, `slice` 25 timed runs/case, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200 files.
`--quick` ⇒ `baseline` 10 / 5, `slice` 5, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 3, `cowboy` 8 connections × 2 s.
The per-runtime loop count `N` in the `foreign`/`list_walk`/`copy_bound`/`gc_bound` cases is **fixed per case** (not profile-dependent), so the `--full` sample counts only widen the estimate, never the loop length (§3.4–3.7).
**Run order is runtime-major**: all `sys` rows execute first, then all `rust` rows — under a *drifting* host load the two sides are measured in different windows (§6.1).

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

`sys` = **system ERTS**, OTP 29.1.1 with the JIT. `rust` = **`beam_rt`**, release, at its **default execution tier**: a working runtime whose per-process tier **defaults to the native baseline JIT** (`JitMode::Auto` ⇒ `Tier::BaselineJit` on the process's first entry, remembered across pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the only kill switch), with anything outside the modelled `genop.tab` surface returning a typed `unsupported`/`BeamError` rather than a wrong result. Its runtime path compiles **per function, lazily** on a function's first entry (`../../crates/beam_engine/src/jit/baseline/code.rs`) — never whole-module; the tier effect and the one-shot cold start are in §3.9.
All tables in this section come from the **focused `xrun`** (`results/m11-bench-xrun.json`, `--full --runtime all`, 1-min load 6.04 → 2.98), which runs the six cross-runtime workloads back to back: **12/12 rows `ok`**.

### 3.1 Startup latency — `baseline` (n = 50, ms)

| stat | sys (system ERTS) | rust (`beam_rt`, default tier) | rust/sys |
|---|---|---|---|
| min | 95.28 | 14.65 | 0.154× |
| **mean** | **112.81** | **15.21** | **0.135× (rust 7.42× faster)** |
| p50 | 113.06 | 15.20 | 0.134× |
| p90 | 122.78 | 15.59 | 0.127× |
| **p99** | **137.51** | **15.92** | **0.116× (rust 8.64× faster)** |
| max | 143.52 | 16.11 | 0.112× |

Rust startup is both lower **and far flatter** (14.7–16.1 ms across 50 samples). This is not bare interpreter boot against a like start: the oracle is a 14-scheduler SMP VM whose boot loses much more to the contended/oversubscribed host, while a `beam_rt` process is a small single-threaded binary that resolves its code path **natively from the filesystem**, with no `erl` spawn (§6.2). The whole-process figures are therefore the trustworthy cross-runtime startup comparison.

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)

| metric | sys (system ERTS) | rust (`beam_rt`) | rust/sys |
|---|---|---|---|
| `compute_loop_ms` p50 | 175.51 | 56.95 | 0.325× |
| `compute_ops_per_sec` | 5 675 957 | 17 049 597 | 3.00× (rust faster) |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 36 221 440 B (34.55 MiB) | **805 458 B (786.6 KiB)** | **44.97× lower** |
| peak RSS (`VmHWM`, whole workload) | 135 761 920 B (129.5 MiB) | 31 793 152 B (30.3 MiB) | 4.27× lower |

The loop is `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'` and runs under **both** runtimes.
**Caveat — read this loop as the native floor, not interpreter throughput.** It is dominated by the natively-served `lists:seq/2` + `lists:sum/1`; the execution-bound signal is §3.4–3.7.
`vm_memory_total_bytes` is a **separate** metric from the OS `peak_memory_bytes` (§6.5) and is never conflated with it: `beam_rt` serves `erlang:memory/0,1` through the `MemoryHook` seam, and the two runtimes are not reporting the same quantity (OTP's `total` includes allocator arenas and preloaded code; `beam_rt`'s is its much smaller in-process heap).

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

Every case printed the **same** stdout under both runtimes (the report's `observed stdout:` note: `add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5 base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, **mean** (ms); per-case peak RSS, **p50** (MiB, base 2):

| case | sys (system ERTS) wall | rust (`beam_rt`) wall | rust/sys wall | sys RSS | rust RSS | sys/rust RSS |
|---|---|---|---|---|---|---|
| `add` (fixture) | 120.66 | 15.35 | 0.127× | 66.39 | 14.50 | 4.58× |
| `fib` (fixture, n=20) | 118.85 | 15.63 | 0.132× | 66.49 | 15.20 | 4.38× |
| `pick` (fixture) | 118.79 | 15.46 | 0.130× | 66.11 | 15.16 | 4.36× |
| `calendar_last_day` | 121.84 | 16.34 | 0.134× | 66.44 | 16.04 | 4.14× |
| `queue_from_size` | 120.53 | 21.02 | 0.174× | 66.46 | 17.99 | 3.69× |
| `gb_trees_size` | 119.78 | 16.56 | 0.138× | 66.34 | 16.01 | 4.14× |
| `base64_encode_size` | 120.58 | 17.23 | 0.143× | 66.27 | 17.20 | 3.85× |
| `ordsets_from_size` | 122.52 | 15.96 | 0.130× | 66.22 | 15.75 | 4.21× |
| `string_str_length` | 121.60 | 37.85 | **0.311×** | 66.36 | 26.30 | 2.52× |

Whole-workload peak RSS: sys **71 741 440 B (68.4 MiB)** / rust **27 758 592 B (26.5 MiB)** (2.58× lower).

**Read the wall column as *whole-process*, not *execution*.** Each case is a single call per process, so the wall time is dominated by process boot; six of the nine also call a *system* module. Per-case `rust` peak RSS **is** the like-for-like per-process memory comparison (one call per process, no loop accumulation): rust **14.5–26.3 MiB** vs sys **66.1–66.5 MiB** (2.52×–4.58× lower).
The `string_str_length` row is the family's **one-shot cold-start** case — the first entry pulls in a large Erlang module graph — and it is the one row well above the ~15–16 ms boot floor (§3.9). **The m10 system-module regression is gone:** m10 rust walls 15.38–48.12 ms → m11 15.35–37.85 ms (`gb_trees_size` 25.12→16.56, `base64_encode_size` 26.68→17.23, `ordsets_from_size` 26.15→15.96, `queue_from_size` 26.00→21.02, `string_str_length` 48.12→37.85; §7.2 and §8 item 7).

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source* (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system `erlc` and executed under both runtimes.
`ns/call` is the startup-cancelled marginal `(mean wall@N − mean wall@0) / N` (n = 10 samples each); `N` is per runtime so both marginals clear the timer noise. `wall_ms` is **not** comparable across runtimes (different `N`); `startup_ms` is.

| case (real function) | N sys / rust | sys ns/call | rust ns/call | rust/sys | startup p50 sys / rust (ms) | RSS p50 sys / rust (MiB) |
|---|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 1 500 000 / 20 000 | 344.97 | 23 751.07 | **68.85×** | 118.62 / 22.99 | 66.64 / 74.53 |
| `cowboy_bstr:capitalize_token/1` | 1 500 000 / 15 000 | 325.00 | 28 757.00 | **88.48×** | 118.70 / 15.42 | 66.12 / 76.95 |
| `emqx_base62:encode/1` | 3 000 000 / 25 000 | 210.23 | 77 167.32 | **367.06×** | 114.42 / 15.33 | 66.28 / 62.68 |
| `emqx_base62:decode/1` | 3 000 000 / 15 000 | 320.82 | 19 306.39 | **60.18×** | 123.04 / 15.13 | 66.49 / 63.51 |

All four cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout (`sys`: `bstr_to_lower=45000000 bstr_capitalize_token=45000000 emqx_base62_encode=48000000 emqx_base62_decode=33000000`; `rust`: `600000 / 450000 / 400000 / 165000` = `N × per_call`).

**This is the execution-bound result.** Startup is comparable while the *per-call* cost of a real foreign function is **~60×–367×** higher under `beam_rt`. The gap is a **real runtime cost, not a harness artifact**: the startup-cancelled marginal is stable and reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`. It is a `beam_rt` per-call host-context cost, not the engine's per-instruction dispatch (§3.8).

> **NEW at m11 — the `foreign` `ns_per_call` row is N-dependent (super-linear) and is therefore not a clean per-call cost at these `N` (recorded, not diagnosed here; see §8 item 1).** Versus m10 the rust figures moved **19 306.39–77 167.32** from **16 325.996–22 566.614** while the `sys` oracle rows are **flat** (210.23–344.97 vs 210.88–339.42) ⇒ **not oracle drift**. The biggest move is `emqx_base62:encode` **+364.83 %** (16 601.05 → 77 167.32); the other three rust rows are **+33.51 %** (`bstr_to_lower`), **+27.43 %** (`bstr_capitalize_token`) and **+18.26 %** (`emqx_base62_decode`). An independent bounded direct probe on this host (default tier, `beam_rt -pa <dir> run <drv.beam> bench_foreign_driver:<fn>/1 <N>`) shows the cost is **super-linear in `N`**: `enc` N=0 → 17 ms, N=2 000 → 48 ms, N=10 000 → 244 ms, N=25 000 → **1 944 ms** (successive marginals ≈ 15.5 → 24.5 → 113 µs/call), while `lower` stays ~linear (~23.9 µs/call at N=25 000). The 1 944 ms reproduces this artifact's own `rust case_emqx_base62_encode_wall_ms` = **1 944.754 ms** (mean of 10 samples at N=25 000). The foreign sources are unchanged (cowboy HEAD `c965d1a`, emqx HEAD `02eaa91831`; both `.erl` files dated 2026-10-05), so it is **not a workload-program change**.
**`foreign` peak RSS is not a like-for-like memory comparison — see §6.7.**

### 3.5 `list_walk` — the per-element dispatch-throughput metric (n = 10/case)

A **BIF-free** body-recursive walk (`walk(N)` sums a 1000-element list `N` times; `build/2` makes the list once per process) runs under both runtimes; the reported figure is the startup-cancelled **ns per walked element**.

| runtime | `N` (walks) | elements walked | `ns_per_element` | `wall_ms` mean | `startup_ms` mean | peak RSS p50 |
|---|---|---|---|---|---|---|
| `sys` (system ERTS) | 100 000 | 100 000 000 | **2.4194** | 360.64 | 118.70 | 66.34 MiB |
| `rust` (`beam_rt`) | 1 000 | 1 000 000 | **45.9731** | 61.41 | 15.44 | 15.21 MiB |

`case_list_walk_elements` (the deterministic, contention-immune companion metric) is `1×10⁸` for sys and `1×10⁶` for rust; each run printed the deterministic constant `N × 500 500` (`rust`: `list_walk=N*500500=500500000`, `sys`: `…=50050000000`).
`ns/element` gives a **19.00×** rust/sys ratio — the narrowest it has ever been (**65.5×** in m9, **22.1×** in m10): the m10 baseline-JIT Y-slot arithmetic + continuation sub-blocks cut the Rust per-element cost **158.83 → 47.39 ns** and m11 shaved it again to **45.97 ns** (−2.99 %, §7.1).

### 3.6 `copy_bound` — the copy boundary: ETF, ETS and process send/receive (**four** cases, n = 10/case)

**Four** copy-boundary cases under both runtimes, each validating its own round-trip with a bound-variable pattern: an **ETF** `term_to_binary`/`binary_to_term` round trip, an **ETS** `ets:insert` + `ets:lookup` round trip, a **two-process send/receive** copy (`send_recv`), and the **delivery-only** variant (`send_recv_plain`, the payload replied directly with **no** ETF leg in the receiver — new at m11). All four are startup-cancelled `ns/op`; the `ops` counter is deterministic.

| case | op | sys ns/op | rust ns/op | rust/sys | ops sys / rust | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|---|
| `etf_roundtrip` | `term_to_binary` / `binary_to_term` | 650.4860 | 7 033.39 | **10.81×** | 1 500 000 / 5 000 | 66.21 | 19.25 |
| `ets_copy` | `ets:insert` + `ets:lookup` | 236.9251 | 2 435.07 | **10.28×** | 2 000 000 / 100 000 | 66.91 | 90.39 |
| `send_recv` | two-process send/receive copy (with the ETF leg) | 1 453.3740 | 21 522.24 | **14.81×** | 500 000 / 2 000 | 66.57 | 21.05 |
| `send_recv_plain` | two-process delivery only (no ETF leg) | 653.7492 | 20 075.24 | **30.71×** | 1 000 000 / 2 000 | 66.64 | 21.16 |

All four ran under both runtimes (`4 ok, 0 unsupported`) with identical stdout (`sys`: `etf_roundtrip=1500000 ets_copy=2000000 send_recv=500000 send_recv_plain=1000000`; `rust`: `5000 / 100000 / 2000 / 2000` = `N × 1`).
`ets_copy` is the cheapest boundary for the oracle, and its rust/sys ratio (10.3×) is **the narrowest in the whole suite together with `etf_roundtrip` (10.8×)**; `etf_roundtrip` fell **15.1× → 10.8×** vs m10 (−28.51 % rust ns/op, a code-attributable move, §7.2) and is now the narrowest copy ratio. **Two-process delivery is now the dearest rust copy boundary**: `send_recv_plain` (the delivery-only case) is **30.71×** the oracle, and `send_recv` (which also runs the ETF leg) is **14.81×** — on rust the whole delivery *is* the cost (the ETF leg adds only ≈1.4 µs/op on rust, 21 522 − 20 075, vs ≈0.80 µs/op on sys, 1 453.37 − 653.75; §8 item 4). `send_recv`'s rust cost fell **72.93 %** vs m10 (79 518.12 → 21 522.24 ns/op), closing the m10 baseline-JIT message-passing imbalance (§7.2).
**`copy_bound` rust peak RSS still grows with `N` — the `ets_copy` row (N = 100 000) reaches 90.39 MiB vs sys 66.91 MiB (§6.7, §8 item 5).** Whole-workload peak RSS: sys **71 970 816 B (68.6 MiB)** / rust **94 973 952 B (90.6 MiB)** (1.32× *higher* for rust).

### 3.7 `gc_bound` — allocate-and-abandon churn (n = 10/case)

`churn(N)` allocates and immediately abandons a 256-element list, a 256-byte binary and a 256-element tuple, `N` times; the reported figure is the startup-cancelled `ns/op`.

| runtime | `N` (churns) | `churn_ns_per_op` | `wall_ms` mean | `startup_ms` mean | peak RSS p50 |
|---|---|---|---|---|---|
| `sys` (system ERTS) | 200 000 | **4 270.1253** | 977.40 | 123.37 | 66.66 MiB |
| `rust` (`beam_rt`) | 2 000 | **128 524.4542** | 272.60 | 15.55 | **15.43 MiB** |

`case_churn_ops` (deterministic) is `200 000` (sys) / `2 000` (rust); each run printed `churn=N*1` (matching its `N`).
The rust/sys ratio is **30.10×**. This row measures the allocate-and-abandon **wall time**; whether a runtime's collector actually runs during the loop is its own admission policy and is **not claimed here** (the report's own note says the same).
The rust peak RSS stays **below** the oracle's (15.43 vs 66.66 MiB p50) and the per-op cost fell a further **12.68 %** vs m10 (147 194.37 → 128 524.45, §7.2).

### 3.8 Reading the cross-runtime numbers

- **Startup / whole-process cost is the trustworthy cross-runtime comparison** (§3.1, §3.3): rust is 7.42× faster at `baseline` startup (mean) and 3.21×–7.86× faster per `slice` case, with 2.58× lower per-process peak RSS. A `beam_rt` process starts in ~15.2 ms.
- **The startup-cancelled marginals are the per-unit execution-throughput signal** (§3.4–3.7), and they point the other way: rust costs **10.3×–367.1×** more per op/call/element. `copy_bound` **10.3×–30.7×**, `list_walk` **19.0×**, `gc_bound` **30.1×** are the stable band (narrower than m10); `foreign` **60.2×–367.1×** is inflated by the N-dependent effect above (§3.4, §8 item 1). That residual is a `beam_rt` per-call runtime cost, not the engine's per-instruction dispatch (next bullet).
- **Only `slice` gives a like-for-like peak-RSS comparison** (§6.7). `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS still grows with the per-runtime loop count `N`.
- The gap is a **`beam_rt` per-call host-context cost**, not `beam_engine` instruction dispatch: the engine's own side of a BIF call is `~11–12 ns` (`../../crates/beam_engine/tests/bif_cost.rs`), i.e. **≲0.15 %** of the runtime's per-call figure, so **~99.85 % is HOST-side** (`beam_rt`'s `ExecutionContext::call_bif`, `../../crates/beam_rt/src/context/calls.rs`). The runtime-side figures are owned by `../../crates/beam_rt/PERF.md` and `../../crates/beam_rt/CONTEXT.md`, not `bench` output.
- **The `slice` whole-process column includes a one-shot cold start, and that cold start is bounded** (§3.9): the runtime path lowers a function on its first entry, so a process that enters a large Erlang module graph pays the first-entry compile cost rather than a whole-module compile.

### 3.9 One-shot cold start, and the **tier effect** (what the JIT default does and does not improve)

Every `slice` case is a single call in a **fresh** `beam_rt` OS process (§3.3), so that wall column carries the cost of entering a system module for the first time. That cost is bounded by construction: the **runtime path** stages a **lazy** native artifact — one sealed mapping per function, lowered on that function's first entry of a `(func, pc)` (`../../crates/beam_engine/src/jit/baseline/code.rs`) — so a fresh process pays only for the functions it actually enters. The **eager** whole-module layout (`JitBackend::compile` / `CodeCache::get_or_compile`) exists for the whole-module introspection surface and for the `jit_bench` harness; it is **not** the runtime path. The module-graph-heavy `string_str_length` row is the family's visible cold-start case (rust 37.85 ms mean vs the ~15–16 ms boot floor, §3.3).

**The tier effect is a Rust-vs-Rust comparison — it is NOT a comparison against the system ERTS.** The default `beam_rt` tier is the native baseline JIT; `BEAM_RT_TIER=interpreter` forces the interpreter. The delta between those two *Rust* columns is the **tier effect** (the JIT making the *Rust* runtime's own interpreter work cheaper, or occasionally dearer). At m11 the control was run as a **matched-form `--quick` pair** — `results/m11-bench-xrun-interp.json` (interpreter) vs `results/m11-bench-xrun-quick.json` (default), archived as `results/m11-compare-tier-effect-quick.txt`:

| workload | metric | rust `BEAM_RT_TIER=interpreter` | rust default (baseline JIT) | interp/default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 17.3247 | 37.5050 | 0.462× (default dearer — see note) |
| `baseline` | `compute_loop_ms` | 63.6089 | 99.0786 | 0.642× (noise; native `lists:seq/sum` floor) |
| `list_walk` | `ns_per_element` | 159.6008 | 47.4465 | **3.36×** (default cheaper) |
| `gc_bound` | `churn` ns/op | 243 176.0348 | 123 688.7103 | **1.97×** (default cheaper) |
| `copy_bound` | `etf_roundtrip` ns/op | 6 759.9803 | 7 496.9099 | 0.90× (interpreter marginally cheaper) |
| `copy_bound` | `ets_copy` ns/op | 3 313.9438 | 2 344.7269 | **1.41×** (default cheaper) |
| `copy_bound` | **`send_recv` ns/op** | 22 654.6925 | **22 054.4402** | **1.03× — parity** |
| `copy_bound` | **`send_recv_plain` ns/op** | 18 662.4402 | **19 330.4250** | **0.97× — parity** |
| `foreign` | 4 cases ns/call | 2 832 121–4 178 832 | 19 191–76 967 | **50×–177×** (default cheaper — **interpreter pathology, see below**) |
| `slice` | 9 cases wall_ms | 20.5–33.8 | 17.1–42.1 | 0.79×–1.49× (mixed; startup noise, no stable tier effect) |

**The requested `copy_bound` rows, with direction:**
- **`send_recv`** (`a` = interpreter): **22 654.6925 → 22 054.4402 ns/op, Δ = −600.25 (−2.65 %)** — the default tier is now marginally *cheaper*; the two tiers are at **parity**.
- **`send_recv_plain`**: **18 662.4402 → 19 330.4250 ns/op, Δ = +667.98 (+3.58 %)** — the default tier is marginally *dearer*; again **parity** (the two cases differ by ~3.6 % in *both* directions, i.e. within run-to-run scatter).

These two cases are the ones with a genuinely matched, same-tier sample count, so the parity reading is solid. The other matched marginals agree on the default tier's direction: `list_walk` `ns_per_element` 159.6008 → 47.4465 (**−70.3 %**, default 3.36× cheaper), `gc_bound` churn 243 176.0348 → 123 688.7103 (**−49.1 %**, default 1.97× cheaper), `copy_bound` `ets_copy` −29.3 % and `etf_roundtrip` **+10.9 %** (the one metric where the interpreter is marginally cheaper). `slice` shows **no stable tier effect**: its per-case `wall_ms` includes a whole process startup per case, and in the matched pair 2 of 9 cases are 32–33 % cheaper under the default tier while 2 are ~25–26 % *dearer*, with the rest inside ±10 % — that spread tracks the per-process startup noise of the two `--quick` runs, not the tier.

> **The m10 "inverted" row is GONE — the `send_recv` tier regression is CLOSED.** At m10 the same matched comparison had `send_recv` at **36 969.39 → 79 518.12 ns/op (default 2.15× *dearer*)**, a repeatable baseline-JIT message-passing cost; a focused `--quick` re-run even reproduced it at **3.31×**. At m11 it is **22 054 vs 22 655 ns/op — parity**. This is a **Rust-vs-Rust tier observation only** — the Rust-vs-system ratios live in the Verdict (c) table, §3.4 and §3.6, and are never the tier numbers. The m10→m11 window's `beam_engine` baseline-JIT **lowered `Send` and the receive-control family natively**, with `beam_message`'s dirty-heap send fast path and `beam_process`'s quiet-queue `service_signals` fast path (`CHANGE.md`, batch 52). **No Rust-vs-system-ERTS win is claimed here** — the `send_recv` rust/sys ratio is still **14.81×** (§3.6).
> **Read the `baseline` rows of this table with care:** the matched `--quick` control happened to sample `startup_ms` (37.5 ms) far above the `--full` cross-runtime run's 15.2 ms and the interpreter run's 17.3 ms, i.e. that row is dominated by one noisy sample set on a shared host, *not* by a tier effect. `compute_loop_ms`'s spread is the same story (the loop is served natively on both sides).
> **`foreign`'s 50×–177× rows are not a "JIT win"** — they are the plain-JIT baseline against the pathological interpreter described next; do not read them as a normal tier effect.

**Why the interpreter control is `--quick` (a measured at-this-commit fact).** At this commit the interpreter tier is **pathologically slow on the `foreign` cross-module calls**, and the cost is **super-linear in `N`**, not a constant multiplier. Direct `beam_rt -pa <scratch> run <drv.beam> <fn>/1 <N>` probes of the case-1 loop give: interpreter N=500 → 102 ms, N=1 000 → 158 ms, N=2 000 → 219 ms, N=4 000 → 711 ms, N=20 000 → **84 088 ms**; default tier N=500 → 35 ms, N=2 000 → 64 ms, N=20 000 → 514 ms. The m10 record's interpreter pass measured this case at **21 959 ns/call** and completed the whole six-workload `--full` tier control in **41 s**; m11 records **4 178 832 ns/call** at N=20 000 (`results/m11-bench-xrun-interp.json`), a **~190×** interpreter regression on this shape, and a `--full` interpreter pass would need **~50 min** of wall time on this host alone. The `--quick` form (3 samples/case) was therefore used for the interpreter control, and the matched-form default-tier run (`m11-bench-xrun-quick.json`) keeps the tier comparison like-for-like. The interpreter tier is **not** on `beam_rt`'s default path (`BEAM_RT_TIER=interpreter` is the only kill switch); this is recorded as a measurement fact, not diagnosed or fixed here.

The **as-specified** artifact `results/m11-compare-tier-effect.txt` (interpreter `--quick` vs the default `--full` `m11-bench-xrun.json`) gives the same directions: `send_recv` 22 654.6925 → 21 522.2389 (−5.00 %), `send_recv_plain` 18 662.4402 → 20 075.2422 (+7.57 %), `list_walk` −71.19 %, `gc_bound` −47.15 %, `foreign` −98.0 % to −99.4 %, and the `sys` rows `only-in-b` by construction.

**Caveat — `jit_bench` pins the hot path, not this path.** The engine's `jit_bench` micro-benchmark drives the **eager** whole-module artifact only, so its interpreter → baseline-JIT speed-ups (`../../crates/beam_engine/MEASURED.md`) describe the hot path with the compile amortised away, and say nothing about the lazy runtime path's per-entry compile cost.
The cited documents are **current-state** records — re-run their own commands for the live figures rather than quoting this summary. And the host-load caveat applies to them exactly as it does here (§6.1).

### 3.10 `beam_rt`'s boot status at this commit (contract A.7)

A factual probe, not a workload: at the measured commit **`erts_rust/target/release/beam_rt -boot bootstrap/bin/start` exits 3** and prints the single stderr line **`beam_rt: unsupported: opcode: run: process blocked (no message available)`**. The real `bootstrap/bin/start.boot` therefore stops at **directive 18 of 22** — **boot is NOT complete** and this document claims no boot completion anywhere. See §6.8.

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS / system toolchain** on the target projects.
The Rust runtime cannot be scheduled on any of them (`Runtimes::SysOnly`, §5), so there is no Rust column — and **none of these numbers is a Rust-runtime result**; they are **reference** figures for the sys-only surfaces.
All absolute values in this section are inflated by the varied host load (§1) and are **not comparable across runs**; §7 states each move explicitly.

### 4.1 `plb` — Programming-Language-Benchmarks (dotnet)

| metric | value |
|---|---|
| status | **`ok`** (the version-aware tool resolution picks `dotnet` 9.0.318) |
| `wall_ms` (two-stage `--task build` + `--task bench`) | 68 318.69 |
| build stage | 23 035 ms |
| `test_time_ms` (n = 10, PLB's own per-test time) | min 0 / mean 1 170.95 / p50 559.98 / p90 3 014.48 / p99 3 355.48 |
| `test_mem_bytes` | 95 862 784 B |
| peak RSS (external `VmHWM` of the `.NET` tool) | 169 824 256 B |
| provenance notes | `resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)`; `rejected dotnet 8.0.425 … (nix-store): requires >= 9` ×2 |

The version-aware resolver (`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` with `MIN_DOTNET_SDK = &[9]`) selects the **greatest** satisfying `dotnet` across `PATH` + `$BENCH_TOOL_PATH` + the store and records every rejection, so a too-old `dotnet` on `PATH` can no longer shadow a newer SDK. The m9 post-fix smoke already proved the defect condition (an 8.0.425 SDK forced first on `PATH`) still resolves 9.0.318; m11's `PATH` resolves ≥ 9 directly.
Read these as **`--quick` observations on a shared host**, not milestone figures: `wall_ms` is dominated by building the kernels' escripts and `test_time_ms` is PLB's own per-kernel time (`min` 0 = a sub-millisecond kernel). They say "PLB builds and runs on this host".

### 4.2 `khepri` — Khepri `rebar3 eunit` (GC / data-structure bound)

`rebar3` 3.27.1 is resolved from the Nix store by the driver's **store scan** — no `PATH` prefix and no env-var opt-in are needed (`rebar3` is not on `PATH`). It runs under **Erlang/OTP 28** (its escript shebang), while the report's `runtimes[]` entry describes the driver's oracle.

| metric | value |
|---|---|
| `rebar3 eunit` wall | 83 402.90 ms |
| peak RSS | 357 818 368 B (341.2 MiB) |

### 4.3 `emqx` — EMQX umbrella build (`make emqx-enterprise-compile`)

`emqx` **does not run**: `skipped(emqx-requires-otp-28)`, with no number fabricated. The blocker is a genuine, mechanical toolchain constraint — **the checkout's exactly-declared pin vs the host's actual runtime**, verified first-hand:

* **The exact pin (required).** `28.5.0.7-2` — `/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`, and `/home/bill/Source/emqx/env.sh:3` → `export OTP_VSN=28.5.0.7-2`. The gate reads only these declared pins (via `required_otp`/`OtpPin` in `src/workloads/emqx.rs`); it never assumes checkout layout.
* **The exact available runtime.** `erl -noshell -eval 'io:format("~s ~s~n",[erlang:system_info(otp_release), erlang:system_info(version)]), halt().'` → `29 17.1`, resolved at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl` (the driver reports both the major and the erts version in the note).
* **The mechanism (why a major-version mismatch is fatal here).** The umbrella build aborts in the `:ezstd` dependency's NIF makefile, which regenerates its `env.mk` with the idiom `erl -noshell -s init stop -eval '…'`. On OTP 29 `init stop` halts **before** the `-eval` runs, so the idiom writes nothing — demonstrated directly on this host: it exits `0` with **no output at all** (on OTP 28 it prints the line). `mix compile` then fails with `No rule to make target '…/env.mk'`. EMQX documents the same constraint in its own `scripts/ensure-rebar3.sh`, whose `case ${OTP_VSN}` accepts only `25*`/`26*`/`27*`/`28*`.
* **Why not "just override the pin".** The only OTP-28 binding in the store (`erlang-28.5.0.7`) cannot be paired with the OTP-29-built **Elixir 1.20.4** / `mix` that the umbrella build's `mix` stage needs, and the pin lives in the **read-only** target checkout (editing the pinned version would be changing the project under test — not an honest measurement). Swapping the whole toolchain is out of this crate's scope.

Recorded in `results/m11-bench-emqx.json` (the skip is immediate, before any build work).

### 4.4 `cowboy` — hello_world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s — `--full`)

`copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk git-fetches cowlib/ranch), starts the release and drives it with the in-process keep-alive loader.

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 45 207.25 ms |
| requests | 953 510 |
| **throughput** | **190 626.72 req/s** |
| errors | 0 |
| latency min / p50 / p90 | 0.026328 / 0.078693 / 0.112260 ms |
| **latency p99** | **0.223380 ms** |
| latency max | 2.872852 ms (n = 20 000) |
| peak RSS | 158 814 208 B (151.5 MiB) |

**vs m10** (also `--full`, 16 conns × 5 s): p50 rose (0.063634 → 0.078693 ms) while **p99 fell** (0.301994 → 0.223380 ms) and throughput fell −9.3 % (210 250 → 190 627 req/s). The in-process loader and the server share one contended machine and nothing about the release build or the loader changed, so this is **host-load variation, not a server change** (§7.3). (`m6-bench-cowboy.json` is a `--quick` run, not comparable.)

### 4.5 `erlc_compiler` — `erlc` over the stdlib + Elixir corpus slice

| metric | value |
|---|---|
| wall | 7 905.65 ms |
| files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| **throughput** | **7.589 files/s** (**5 882.25 LOC/s**) |
| peak RSS | 135 045 120 B (128.8 MiB) |
| notes | `corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)` |

The `files_per_sec`/`loc_per_sec` rates are computed from the files/LOC `erlc` actually **produced**, not from what was attempted — so a partial run cannot overstate them. Here all 60 attempted files produced a `.beam`, so produced = attempted. The rate is ~14 % below m10's (8.842 files/s) with the **same 60/60 files and the unmodified system `erlc`**, so the move is host-load-attributable (§7.3).

### 4.6 `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

Measures the **installed** `elixirc` (the in-repo checkout is unbuilt and read-only) over a staged `.ex` corpus (`--full` ⇒ up to 200 files).

| metric | value |
|---|---|
| wall | 47 667.62 ms |
| files attempted / `.beam` produced | 105 / **223** (97/105 compiled) |
| **throughput** | **4.678 files/s** (**2 348.85 LOC/s**) |
| compiler startup | 324.33 ms |
| peak RSS | 205 479 936 B (196.0 MiB) |
| notes | `batch compile failed; per-file fallback (version skew)`; `compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`; `223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)` |

**`files_attempted` (105) < `files_compiled` (223) is expected, not a bug:** one Elixir source may define several modules, so the produced `.beam` count can exceed the source count. The throughput rate uses the produced count.

## 5. Workloads that did **not** run in the m11 measurement run — verified reasons

| workload | status | verified reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`) while the host `erl` is OTP 29.1.1 / erts-17.1; the `:ezstd` `env.mk` make-target's `erl -noshell -s init stop -eval …` idiom produces nothing under OTP 29, so `mix compile` aborts. Not satisfiable without a full OTP-28 toolchain swap (§4.3). No number is fabricated. |
| every `SysOnly` workload scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/bootstrap an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. (Not exercised as a row in m11 — the m11 sys-only runs used `--runtime sys` — but it is the harness behaviour.) |

## 6. Reproducibility, stability and known measurement caveats

### 6.1 The host is shared and loaded, and the two runtimes are measured in different windows
The 1-min `/proc/loadavg` across the m11 session ran from **1.12 to 7.18** against a 14-core host (§1, `results/m11-load-record.md`); the cross-runtime pass started at 6.04 and **fell** to 2.98, so its later (`rust`) side ran under a *lighter* load, while the external-target and tier-control passes ran at the quieter end (1.36 → 2.36 and 1.81 → 2.59). The runner is **runtime-major** (`sys` rows first, then `rust` rows), so under a drifting load the two sides see different conditions. Loadavg is host-wide and does **not** move monotonically with the measured figures (§7), so prefer ratios, prefer the startup-cancelled rows, and always record `/proc/loadavg` alongside an absolute figure.

### 6.2 `beam_rt` resolves its code path natively from the filesystem
The runtime locates its system `ebin` directories (stdlib, kernel, erts) with a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`: `$ERTS_STDLIB_DIR` when set, else the standard OTP installation layout under the `erl` on `$PATH` — *resolved to a path, never executed* — then this executable's ancestors, `$ERL_TOP`, and standard prefixes). The system `erl` is only *run* as a last resort, for an installation that layout cannot see. That is why rust startup is ~15 ms per process. Setting `$ERTS_STDLIB_DIR` still short-circuits the probe; `bench` never sets it (the harness is a pure driver/observer and never mutates the runtime's environment beyond argv). The per-element/per-call metrics cancel whatever startup cost remains by construction.

### 6.3 `elixir_compiler` measures the installed `elixirc` 1.20.4
against the unbuilt, read-only 1.21.0-dev checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the artifact.

### 6.4 `cowboy` re-fetches cowlib/ranch in a fresh scratch copy on every run (network)
which is why its `build_ms` (~45.2 s) dominates the workload.

### 6.5 Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`)
Erlang's own `erlang:memory(total)` is a **separate** metric (§3.2) and is never conflated with it.

### 6.6 `foreign` / `list_walk` / `copy_bound` / `gc_bound` `wall_ms` are not comparable across runtimes
Each side runs a different `N`. Compare the startup-cancelled marginals (`ns_per_call`, `ns_per_element`, `ns_per_op`) and `startup_ms`. **At m11 the `foreign` `ns_per_call` row is additionally N-dependent (super-linear) and is therefore not a clean per-call cost at these `N`** (§3.4, §8 item 1).

### 6.7 The rust peak RSS of `foreign` / `list_walk` / `copy_bound` / `gc_bound` still grows with the loop count `N` — it is **not** a like-for-like memory comparison
`beam_rt` has a process-heap collector driven from `crates/beam_rt/src/context/gc.rs` at the scheduled-slice heap-admission point, and the BIF-result copy-out + pooled-heap rewind (`crates/beam_rt/src/context/calls.rs`) bounds the **BIF-churn** shape — `gc_bound`'s rust peak RSS is **15.43 MiB p50** (m10: 15.40) and `list_walk`'s **15.21 MiB**. But the **`copy_bound` `ets_copy`** row still reaches **90.39 MiB** at N = 100 000 vs sys 66.91 MiB (m10: 90.30 MiB — it has *not* fallen), and `foreign`'s rust RSS (74.5–77.0 MiB for the two `cowboy_bstr` cases) still tracks its N. `beam_rt` peak RSS therefore still scales with `N` on those paths; **the cause of the `ets_copy` growth is not identified here** (§8 item 5 — the m10 ETS-adopt explanation was refuted).
**Consequence:** use `slice` (§3.3, one call per process, `N`=1) as the comparable per-process metric, and treat the loop workloads' rust RSS as loop-length-dependent (§8 item 5).

### 6.8 Contract A.7 — the `beam_rt` boot status at this commit is **incomplete**
At the measured commit `erts_rust/target/release/beam_rt -boot bootstrap/bin/start` **exits 3** and emits exactly one stderr line:
```
beam_rt: unsupported: opcode: run: process blocked (no message available)
```
The real `bootstrap/bin/start.boot` therefore **stops at directive 18 of 22** — the run does **not** reach the final `{progress,started}`. No part of this document claims a completed boot. `bench`'s own workloads exercise only the plain per-process `-eval`/`run` startup path (which does complete); contract A.7's boot completion is tracked in the differential gate, not here.

## 7. Deltas vs the previous run (`m10`, and `m9`/`m6`/`m5`)

Every figure below is quoted from the committed artifacts: `m11-*` (2026-10-14 05:47–06:57 UTC, commit `ab6f1b5e58`, full-pass load 6.04 → 2.98 / 1.36 → 2.36 / 1.81 → 2.59), `m10-*` (2026-10-13 23:06–23:15 UTC, load 5.43 → 7.59 / 7.94 → 8.03 / 21.12 → 10.30), `m9-*` (2026-10-13 13:25–14:10 UTC, load 11.01 → 6.02 / 4.68 → 4.43), `m6-*` (2026-10-11, cross-runtime load ≈414–440), `m5-*` (2026-10-09, load ≈90–153) and `m4-*` (2026-10-08, load 4.77 → 11.59). **The runs sit at very different — and non-monotonic — host loads.**
The tool-generated two-file diffs are archived: `results/m11-compare-vs-m10-xrun.txt` and `results/m11-compare-vs-m10-external.txt`.

**Cross-run comparability.** Only the **load-invariant** rows are safe to compare across runs: the startup-cancelled marginals and, above all, the **deterministic counters** (`*_ops`, `*_elements`). The absolute wall / startup / throughput rows are **not** cross-run comparable — they moved in *both* directions between runs — which is why `/proc/loadavg` is treated as a weak proxy throughout this document.

### 7.1 m11 vs m10 — the cross-runtime marginals (rust side)

Rust `beam_rt` cells, m10 → m11 (mean; bigger-is-better negative deltas are improvements):

| metric (mean) | m10 | m11 | move |
|---|---|---|---|
| `copy_bound` `send_recv` `ns_per_op` | 79 518.1199 | **21 522.2389** | **−72.93 %** |
| `copy_bound` `send_recv` `wall_ms` | 175.8221 | **58.4826** | −66.74 % |
| `copy_bound` `etf_roundtrip` `ns_per_op` | 9 838.0045 | **7 033.3943** | **−28.51 %** |
| `copy_bound` `ets_copy` `ns_per_op` | 3 267.8549 | **2 435.0664** | −25.48 % |
| `gc_bound` `churn` `ns_per_op` | 147 194.3719 | **128 524.4542** | −12.68 % |
| `list_walk` `ns_per_element` | 47.3880 | **45.9731** | −2.99 % |
| `foreign` `bstr_to_lower` `ns_per_call` | 17 789.3946 | **23 751.0676** | **+33.51 % (worse)** |
| `foreign` `bstr_capitalize_token` `ns_per_call` | 22 566.6138 | **28 757.0043** | **+27.43 % (worse)** |
| `foreign` `emqx_base62_decode` `ns_per_call` | 16 325.9957 | **19 306.3883** | **+18.26 % (worse)** |
| `foreign` `emqx_base62_encode` `ns_per_call` | 16 601.0457 | **77 167.3229** | **+364.83 % (worse)** |
| `baseline` `startup_ms` | 15.5243 | **15.2122** | −2.01 % |
| `baseline` `vm_memory_total_bytes` | 805 458 | 805 458 | unchanged |
| `copy_bound` `send_recv_plain` (all rows) | — | — | `metric-only-in-b` (the new case) |

The **sys (system ERTS)** side is unchanged within load: `baseline` `startup_ms` 114.3603 → 112.8126 (−1.35 %), `foreign` `ns_per_call` 210.88–339.42 → 210.23–344.97, `list_walk` `ns_per_element` 2.1431 → 2.4194, `copy_bound` `ns_per_op` 240.47–1 453.98 → 236.93–1 453.37, `gc_bound` `churn` 4 364.91 → 4 270.13. That flatness is the cross-check that the interesting rust moves are not oracle drift.

### 7.2 Real, code-attributable changes since m10

* **`copy_bound send_recv` fell 72.93 % on the rust side** (79 518.12 → 21 522.24 ns/op, and 3.7× cheaper) — this **closes the m10 baseline-JIT message-passing imbalance (§3.9)**: the baseline tier at this commit **lowers `Send` and the receive-control family natively**, and `beam_message`'s dirty-heap send fast path and `beam_process`'s quiet-queue `service_signals` fast path (`CHANGE.md`, batch 52) back it. The same change drives the new `send_recv_plain` case (20 075.24 ns/op) into the same band.
* **`copy_bound etf_roundtrip` fell 28.51 %** (9 838.00 → 7 033.39 ns/op) and `ets_copy` **−25.48 %** (3 267.85 → 2 435.07) — the same window's cut of the registered-BIF dispatch prologue + BIF-result copy-out (`beam_rt`).
* **`gc_bound` churn `ns_per_op` fell 12.68 %** (147 194.37 → 128 524.45) with the churn rust peak RSS flat/low (15.43 MiB p50) — the BIF-result copy-out + pooled-heap rewind (`beam_rt`; `context/calls.rs`).
* **`slice`'s system-module per-process cost recovered** (m10 rust walls 15.38–48.12 ms → m11 15.35–37.85 ms): `gb_trees_size` 25.12 → 16.56, `base64_encode_size` 26.68 → 17.23, `ordsets_from_size` 26.15 → 15.96, `queue_from_size` 26.00 → 21.02, `string_str_length` 48.12 → 37.85. The m10 item-6 regression is therefore **closed**.
* **`list_walk` per-element `ns_per_element` fell a further 2.99 %** (47.39 → 45.97 ns) — the m10 baseline-JIT Y-slot arithmetic + continuation sub-blocks; the tier table (§3.9) shows the default tier still **3.36× cheaper** than the interpreter on this shape.
* **`beam_process` / `beam_engine` call seams** continue to move (single-resolution `call_ext` seam, fingerprint+address memo, in-block `Send`/receive lowering); the bench-visible expression is the `copy_bound`/`send_recv_plain` band above. In-crate figures live in `crates/beam_engine/MEASURED.md`.

### 7.3 Load-attributable / not comparable

* **`baseline` startup, `sys` side.** m10 mean 114.36 ms → m11 mean 112.81 ms — essentially flat, host variation, not a code change.
* **`sys`-side marginals.** `foreign` ns/call +4.2 % / −0.3 % / +9.2 % / +1.6 %; `list_walk` ns/element +12.9 %; `gc_bound` churn −2.2 %; `copy_bound` ns/op −1.5 % to −0.0 % — the cross-runtime pass started at load 6.04 and fell to 2.98, so these small moves are within host variation.
* **Cowboy.** p50 +23.66 % (0.063634 → 0.078693) but p99 −26.03 % (0.301994 → 0.223380) and throughput −9.33 % (210 250.49 → 190 626.72) — host load, per §4.4 (a rising p50 with a *falling* p99 reads as contention, not a server change).
* **Compiler throughput / `khepri`/`plb` wall.** `erlc_compiler` −14.17 % (8.8424 → 7.5895 files/s, same 60/60 files), `elixir_compiler` +0.29 % (4.6646 → 4.6782 files/s, same 105/223, though `compiler_startup_ms` +23.77 %), `khepri` +5.89 % (78 762.66 → 83 402.90 ms), `plb` −3.56 % (70 841.43 → 68 318.69 ms). All ran the unmodified system toolchain, so these moves are not code-attributable.

### 7.4 Unchanged / still-open

* **`baseline` `vm_memory_total_bytes` (rust)** — m10 805 458 B → m11 805 458 B (the two runtimes still report different quantities, §3.2).
* **`foreign` peak RSS still scales with `N`**, and `copy_bound`'s `ets_copy` row reads **90.39 MiB** (m10: 90.30) at N = 100 000 (§6.7, §8 item 5).
* **`list_walk` and `gc_bound` rust peak RSS are *below* the oracle's** (15.21/15.43 MiB vs 66.34/66.66 MiB), but they are still loop-length-dependent rows, not like-for-like.
* **The per-unit execution-throughput gap is the stable cross-runtime story** — but at m11 its `foreign` slice is confounded (below). `copy_bound` `etf_roundtrip`: `m9` 118.5× → `m10` 15.1× → `m11` **10.81×**; `copy_bound` `send_recv`: `m9` 114.4× → `m10` 54.7× → `m11` **14.81×**; `copy_bound` `ets_copy`: `m10` 13.6× → `m11` **10.28×**. `list_walk`: `m5` 66.6× → `m6` 108.7× → `m9` 65.5× → `m10` 22.1× → `m11` **19.0×**. `gc_bound churn`: `m9` 42.2× → `m10` 33.7× → `m11` **30.1×**. `foreign`: `m4` (quiet) 53.8×–72.0× → `m5` 53.1×–96.0× → `m6` 74.9×–131.4× → `m9` 46.2×–74.2× → `m10` 52.4×–78.7× → `m11` **60.2×–367.1×** (the last is the N-dependent effect above, not a clean ratio).

## 8. Fresh ranked bottleneck list (from the m11 data)

Derived from the m11 measurements above (each item: measured m11 cost → where to look → expected win). Each ratio names **both sides**: `rust` = `beam_rt` (default baseline-JIT tier) vs `sys` = system ERTS, unless the row is explicitly a Rust-vs-Rust tier row.
Two of the m10 items are **closed** and are recorded as such: **m10 item 2 (the baseline-JIT `send_recv` tier regression) is CLOSED** (§3.9 — the default tier is now at parity, 0.97×–1.03×), and **m10 item 6 (the `slice` system-module per-process cost) RECOVERED** (§3.3, §7.2) — so neither is open any more, contrary to what one might assume from the m10 list. What remains, ranked by measured headroom:

1. **`foreign` `ns_per_call` is N-dependent (super-linear) in BOTH tiers — NEW and the top item. Recorded, explicitly NOT diagnosed here (outside this node; reported upward).**
   Rust `foreign` `ns_per_call` is **19 306.39–77 167.32** ns/call at m11 vs **16 325.996–22 566.614** at m10, while the `sys` oracle rows are **flat** (210.23–344.97 vs 210.88–339.42) ⇒ not oracle drift. `emqx_base62:encode` **+364.83 %** (16 601.05 → 77 167.32); `bstr_to_lower` **+33.51 %**; `bstr_capitalize_token` **+27.43 %**; `emqx_base62_decode` **+18.26 %**.
   The cost is **N-dependent (super-linear) in both tiers**. Independent bounded direct probe on this host (default tier, `beam_rt -pa <dir> run <drv.beam> bench_foreign_driver:<fn>/1 <N>`): `enc` N=0 → 17 ms, N=2 000 → 48 ms, N=10 000 → 244 ms, N=25 000 → **1 944 ms** (successive marginals ≈ 15.5 → 24.5 → 113 µs/call ⇒ super-linear in N), while `lower` stays ~linear (~23.9 µs/call at N=25 000). The 1 944 ms reproduces the artifact's own `rust case_emqx_base62_encode_wall_ms` = **1 944.754 ms** (mean of 10 samples at N=25 000). The interpreter tier on `foreign` is **~190× worse than m10** and also super-linear (probe: N=2 000 → 219 ms, N=4 000 → 711 ms, N=20 000 → **84 088 ms** ⇒ `m11-bench-xrun-interp.json` records **4 178 832 ns/call** at N=20 000 vs m10's 21 959.53); this is *why* the m11 tier control had to run `--quick` (a `--full` pass ≈ 50 min).
   *Consequence for reading m11:* the `foreign` `ns_per_call` row at these `N` is **not a clean per-call cost**, and the m11-vs-m10 `foreign` jump is **at least partly this effect**.
   *Evidence it is not a workload-program change:* the foreign sources are unchanged (cowboy HEAD `c965d1a`, emqx HEAD `02eaa91831`; both `.erl` files dated 2026-10-05).
   *Lead:* the cross-module call path as exercised by `-pa`-loaded scratch modules; the commits in the m10→m11 window are the baseline-JIT code-cache **digest key + entry-path priming** and the in-block **`Send`/receive lowering** (`beam_engine`), and the registered-BIF **dispatch prologue + BIF-result copy-out** cuts (`beam_rt`) — it needs an A/B at m10's commit (`b4593818b3`).
2. **Per-unit Erlang execution throughput — the stable gap vs system ERTS (10.3×–30.7× on the clean rows).** `copy_bound` **2 435.07–21 522.24** vs **236.93–1 453.37** ns/op (**10.28×–30.71×**), `list_walk` **45.9731** vs **2.4194** ns/element (**19.00×**), `gc_bound` churn **128 524.45** vs **4 270.13** ns/op (**30.10×**). This is **narrower than m10's 13.6×–54.7× / 22.1× / 33.7×**. (`foreign`'s 60.2×–367.1× belongs to item 1, not here.)
   *Lead:* the engine step loop (`erts_rust/crates/beam_engine/src/machine/drive.rs`) and baseline-JIT block coverage (`erts_rust/crates/beam_engine/src/jit/baseline/emit/`), plus the host BIF seam (`erts_rust/crates/beam_rt/src/context/calls.rs::call_registered`, ~line 122) — the engine's own side is only ~11–12 ns of the per-call cost, so ~99.85 % is **host-side** (`crates/beam_engine/tests/bif_cost.rs`).
   *Expected win:* the largest single lever in the suite; the m10/m11 JIT work already took `list_walk` from 65.5× (m9) to 19.0×.
3. **`gc_bound` churn is still 30.10× dearer (128 524.45 ns/op) — the memory half is flat/low, the allocation wall cost is the residual.** m11 rust peak RSS is **15.43 MiB** p50 (m10: 15.40) and the per-op churn cost fell **12.68 %**.
   *Lead (unchanged):* the copy-out + rewind seam (`erts_rust/crates/beam_rt/src/context/calls.rs::move_result_into_process_heap`, ~line 674; the rewind guard in `call_registered`) — every `churn` iteration builds three containers and each container-returning BIF result is copied into the process heap.
   *Expected win:* a cheaper copy-out (bulk word copy, avoiding a re-walk) applies to *every* container-returning BIF, so it should show up on `churn`, `ets_copy` and the module-call cases together.
4. **Two-process delivery is the dearest rust copy boundary.** The **new** `send_recv_plain` case is **20 075.24 ns/op** (**30.71×** the sys 653.75) while `send_recv` (which also runs the ETF leg) is **21 522.24 ns/op** (**14.81×** the sys 1 453.37). On rust the whole delivery *is* the cost — the ETF leg adds only **≈1.4 µs/op** (21 522 − 20 075) versus **≈0.80 µs/op** on sys (1 453.37 − 653.75).
   *Lead:* `erts_rust/crates/beam_message`'s copy engine + `erts_rust/crates/beam_process`'s signal delivery / scheduler wake, reached through the engine's `send`/`receive` seams.
   *Expected win:* bring the delivery cost toward the oracle's; the m10→m11 window already showed this surface is responsive (the tier's `send_recv` fell 72.93 %, §7.2).
5. **`ets_copy` rust peak RSS still grows with `N`: rust 90.39 MiB at N = 100 000 vs sys 66.91 MiB** (m10: 90.30 MiB — it did **not** fall). The symptom is open; **the m10 "ETS-adopt path" explanation is REFUTED** (by batch 52's bisection), and **the cause of the growth is unidentified**. Do not restate the ETS-adopt lead as if it were still live.
   *Lead:* none claimed. The churn fix did not cover the ETS insert/lookup loop, and the mechanism by which the loop accumulates on the process heap is unknown here.
   *Expected win:* make `ets_copy`'s per-op memory flat like `gc_bound`'s; it is the last loop whose peak RSS *rises* above the oracle's.
6. **`copy_bound etf_roundtrip` improved 28.51 % (9 838.00 → 7 033.39 ns/op) and is now the narrowest copy ratio (10.81× vs sys 650.49)** — de-prioritised; it is in the stable band together with `ets_copy` (10.28×).
7. **m10's item-6 `slice` system-module per-process regression is CLOSED.** m10 rust walls 15.38–48.12 ms → m11 **15.35–37.85 ms** (`gb_trees_size` 25.12→16.56, `base64_encode_size` 26.68→17.23, `ordsets_from_size` 26.15→15.96, `queue_from_size` 26.00→21.02, `string_str_length` 48.12→37.85). `string_str_length` remains the single cold-start case (lazy per-function compile, rust 37.85 ms mean vs the ~15–16 ms floor) — a **low-priority refinement, not a hole**.
8. **External sys-only targets remain reference-only** — no Rust-runtime row exists for them (§4, §5); `emqx` is the one exact mechanical skip (`emqx-requires-otp-28`).

## Honesty audit — measurement-integrity notes

No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always `failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`, `src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`/non-doc `unreachable!`, no `#[ignore]`.

- **`foreign` classifies a staging failure honestly** (`src/workloads/foreign.rs`: `StageError` + `stage_outcome`): a checkout whose foreign `.erl` sources are genuinely absent ⇒ `skipped(sources-missing)`, while a failure to compile the sources we *did* stage (the embedded driver or a present foreign source) ⇒ `failed(module-compile-failed)` — matching the workloads whose sources are ours (`list_walk`/`copy_bound`/`gc_bound`). `slice` treats a failed fixture rebuild as `skipped(fixture-missing)`.
- **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` are computed from the *produced* counts**, so a partial `erlc` run cannot overstate throughput next to `files_compiled`. `elixir_compiler` likewise uses the produced count.
- **A version-constrained tool is resolved by version, and the choice is auditable.** `plb` calls `tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` (min `9`), which probes every candidate's `--version` and selects the greatest satisfying one. The resolved binary + version + source and every rejected candidate's reason are recorded in the row's `notes` — in **both** the success and the build-failure branch (§4.1). `khepri`'s `runtimes[]` version describes the driver's oracle, not necessarily the Erlang the store `rebar3` escript chose (§4.2).
- `proc.rs`'s drain-thread join (`h.join().unwrap_or_default()`) is infallible **by construction** — the drain threads run only `read_to_end` + a lossy UTF-8 conversion, neither of which can panic — so the `unwrap_or_default` masks nothing real; the invariant is documented at the call site.
- `baseline.rs`'s docs correctly state that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its `MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`; `is_capability_gap` survives only as a fallback classifier.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` startup-cancelled methodology is implemented exactly as this document states (startup-cancelled `(mean wall@N − mean wall@0) / N`). The runtime-side hot-path figures (registered-BIF dispatch ≈ 79–96 ns/call; `pb:tr/2`'s `trunc/1` ≈ 76 ns/iter) are `beam_rt` measurements (`../../crates/beam_rt/PERF.md`, `../../crates/beam_rt/TRACE_AND_PERF.md`), **not** produced here.
- **Boot status is recorded as fact, not as a metric:** `beam_rt -boot bootstrap/bin/start` exits `3` on `opcode: run: process blocked (no message available)` at directive 18 of 22 (§3.10, §6.8). No boot completion is claimed anywhere.
- **The m11 tier control is a `--quick` run**, and its `foreign` rows are the pathological interpreter (§3.9); the **matched-form** default-tier `--quick` control (`m11-bench-xrun-quick.json`) exists precisely so the tier comparison is like-for-like, and the mismatched `m11-compare-tier-effect.txt` is labelled a form mismatch. Every tier table is labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system ERTS) and `rust` (`beam_rt` default tier) explicitly.
- **`m11` pass integrity:** the driver and `beam_rt` were built from commit `ab6f1b5e58835cbf5fcb75257679532c46090a74`; every run's stdout was checked against the deterministic constant on both sides; the tier control is a separate artefact (`m11-bench-xrun-interp.json`, plus the matched `m11-bench-xrun-quick.json`) so the Rust tiers cannot be conflated with the `sys` column; all committed artifact paths are normalised (`runtimes[].program` → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`).

**Reproducibility note (PLB / tool resolution).** `plb`'s outcome is *tool- and environment* state, not repository state. The resolver picks the **greatest `dotnet` ≥ 9.0** across `PATH` + `$BENCH_TOOL_PATH` + the Nix store, so `plb` runs wherever any 9.x+ SDK is reachable — even with an older `dotnet` first on `PATH`. `khepri` needs only the `rebar3` store entry and runs as-is.

## Archived artifacts

| file | contents |
|---|---|
| `results/m11-bench-xrun.json` | **current cross-runtime run**: `--full --runtime all`, the six `Runtimes::Both` workloads × both runtimes, 12/12 rows `ok` (load 6.04 → 2.98, wall 2m54s) — the source of §3 |
| `results/m11-bench-xrun-interp.json` | **the tier-effect control**: the same six workloads, `rust` only, `BEAM_RT_TIER=interpreter`, 6/6 rows `ok` — **profile `quick`** (the interpreter tier is pathological on `foreign` at this commit; §3.9), load 1.81 → 2.59, wall 12m27s. **Rust-vs-Rust only; no `sys` rows.** |
| `results/m11-bench-xrun-quick.json` | **supplementary matched-form default-tier control**: the same six workloads, `rust` only, default (baseline-JIT) tier, `--quick` — the same form as the interpreter control, so the tier table is read off two `--quick` runs (load 2.38 → 2.16, wall 17s). **Rust-vs-Rust only.** |
| `results/m11-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside `m11-bench-xrun.json` (74 numeric rows; the tool diffs the **mean**) |
| `results/m11-compare-tier-effect.txt` | `bench compare m11-bench-xrun-interp.json m11-bench-xrun.json` — Rust interpreter (`--quick`) → Rust default (`--full`); **form mismatch** (`a` = interpreter) |
| `results/m11-compare-tier-effect-quick.txt` | `bench compare m11-bench-xrun-interp.json m11-bench-xrun-quick.json` — the **matched-form** `--quick` ↔ `--quick` tier control (§3.9) |
| `results/m11-compare-vs-m10-xrun.txt` / `results/m11-compare-vs-m10-external.txt` | the tool's two-file diffs against the m10 record — the source of §7 |
| `results/m11-bench-external.json` | the external sys-only targets in one `--full` report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri` (§4.2, §4.4–§4.6; load 1.36 → 2.36, wall 3m11s) |
| `results/m11-bench-plb.json` | `plb` → `ok`, `wall_ms` 68 318.69, `dotnet` 9.0.318 resolved from `PATH` (§4.1) |
| `results/m11-bench-emqx.json` | EMQX → `skipped(emqx-requires-otp-28)` (§4.3) |
| `results/m11-load-record.md` | the m11 pass: commit measured, exact commands, the `/proc/loadavg` before/after each run, per-workload status, per-case detail, the tier-effect table and the boot-status probe |
| `results/m11-tool-targets-notes.md` | the factual per-target status/blocker notes and the environment findings |
| `results/m10-*` | the **previous** run (2026-10-13 23:06–23:15 UTC): the cross-runtime report + `BEAM_RT_TIER=interpreter` control, the per-target reports, the cross-runtime/tier/vs-m9 diffs, and the `m10-load-record.md`/`m10-tool-targets-notes.md` records — **regression baseline for §7** |
| `results/m9-*` / `results/m6-*` / `results/m5-*` / `results/m4-*` | the runs before that (2026-10-13 / 2026-10-11 / 2026-10-09 / 2026-10-08), kept as earlier regression baselines |

The archived copies are the harness output except for two mechanical normalisations that strip environment-specific absolute paths: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` is replaced with `$TMPDIR/`. **No metric value is altered.**
