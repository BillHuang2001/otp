# Rust-ERTS benchmark results — current measurement run (m6)

Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured, never modelled**.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-11, 00:43–01:02 UTC**, at the tree archived in commit `844220c945`.
Raw harness JSON is committed under `results/` as `m6-*`; the previous runs are kept as `m5-*` (2026-10-09) and `m4-*` (2026-10-08) and are the baselines for the delta section (§7).

## Verdict

**(a) / (b) Which targets run, which skip.** Five of the six external target projects run; one skips with a verified toolchain reason.

| target project | runs? | result / verified reason |
|---|---|---|
| `plb` (Benchmarks Game kernels, .NET) | **runs** | `ok` — `wall_ms` 125 424.71 / 82 557.97 (two independent runs, §4.1) |
| `khepri` (tree DB) | **runs** | `ok` — `rebar3 eunit` wall 101 386.88 / 81 362.09 ms (§4.2) |
| `cowboy` (HTTP server) | **runs** | `ok` — 40 684.14 req/s, p99 latency 1.628 ms (§4.4) |
| `erlc_compiler` (Erlang compiler) | **runs** | `ok` — 6.236 files/s over the stdlib+Elixir corpus (§4.5) |
| `elixir_compiler` (Elixir compiler) | **runs** | `ok` — 1.796 files/s (§4.6) |
| `emqx` (MQTT broker) | **skips** | `skipped(emqx-requires-otp-28)` — EMQX pins OTP 28, the host `erl` is OTP 29 (§4.3, §5) |

The six workloads the Rust runtime itself can host (`Runtimes::Both`) — `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound` — all ran: **12/12 rows `ok`** (0 skipped, 0 unsupported, 0 failed). Every other workload is `SysOnly` and would report `skipped(runtime-unsupported)` under `rust` (§5).

**(c) Ratio to the system-ERTS oracle, per cross-runtime workload.** `rust/sys` is the Rust cost as a multiple of the system cost (`<1` ⇒ Rust cheaper, `>1` ⇒ Rust dearer). Values are the m6 `xrun` (§3, `--full`, load 434.54 → 415.14).

| workload | surface | headline metric | sys | rust | rust/sys | reading |
|---|---|---|---|---|---|---|
| `baseline` | startup / CLI | `startup_ms` (mean, whole process) | 157.09 ms | 17.50 ms | **0.11×** | Rust **8.98× faster** per process |
| `slice` (9 programs) | compute | `case_*_wall_ms` (mean, per case) | 136.22–228.73 ms | 17.92–36.90 ms | **0.078–0.241×** | Rust **4.16×–12.77× faster** |
| `foreign` (Cowboy/EMQX source) | compute / `.beam` | `case_*_ns_per_call` | 216.54–423.66 ns | 27 184.74–41 393.44 ns | **74.9×–131.4×** | Rust **74.9×–131.4× dearer** per call |
| `list_walk` | dispatch | `case_list_walk_ns_per_element` | 3.1312 ns | 340.5021 ns | **108.7×** | Rust **108.7× dearer** per element |
| `copy_bound` (ETF / ETS / send-recv) | Msg / copy | `case_*_ns_per_op` | 244.59–1 452.94 ns | 4 281.73–194 813.33 ns | **17.5×–188.4×** | Rust **17.5×–188.4× dearer** per op |
| `gc_bound` (allocation churn) | GC | `case_churn_ns_per_op` | 4 643.26 ns | 258 354.39 ns | **55.6×** | Rust **55.6× dearer** per op |

- **The two directions are both real and answer different questions.** Rust's *whole-process cost* is far lower (a `beam_rt` process is small and starts in ~17–37 ms), while the *per-unit execution cost* of Erlang code under `beam_rt` is far higher (the per-unit execution-throughput gap: **17×–188×**, §3.8). Startup is the trustworthy whole-process comparison; the startup-cancelled marginals are the per-unit execution-throughput signal.
- **The one-shot cold start is bounded by per-function lazy compilation** (§3.9). A `slice` sample is a single call in a fresh process, and a module-graph-heavy case whose first entry pulls in a large Erlang module graph (`string_str_length`-style) sits at **~1.2×** the interpreter tier, not at a whole-module compile.
- **Peak RSS is a like-for-like cross-runtime comparison only for `slice`** (one call per process). For `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS grows with the per-runtime loop count `N` and is **not** comparable (§6.7); `slice` shows rust **2.65× lower** than sys.
- **Load caveat (read before quoting any absolute number).** The cross-runtime run was measured at 1-min `/proc/loadavg` ≈ **414–440** on a shared 14-core host; the sys-only target runs swung between **3** and **>600** (§1). Absolute latencies are inflated by contention and are **not comparable across runs**; prefer the ratios above, the startup-cancelled marginals and the deterministic counters.

## Coverage — workload ↔ rewrite surface

The driver now exercises the messaging/copy, ETF, ETS and GC surfaces named in the top-level `ERTS_RUST_REWRITE_TEST_PROJECTS.md` coverage matrix, **under both runtimes**, via the two new workloads `copy_bound` / `gc_bound`; the I/O, compute and compiler surfaces were already covered (the I/O and compiler ones only on the system runtime).

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

> The benchmark host is a **shared 14-core machine** and was heavily oversubscribed during m6.
> The 1-minute `/proc/loadavg` for the cross-runtime run was **≈414–440** (≈30× the core count); the sys-only target runs swung between **3** and **>600** (§1 records every reading).
> Every *absolute* latency / wall / throughput below is therefore inflated by CPU contention and is **not comparable across runs**. Prefer **ratios**, the **startup-cancelled per-op / per-call / per-element marginals** (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counters** (`*_ops`, `*_elements`).
> `/proc/loadavg` is host-wide and a weak proxy — it does **not** move monotonically with the measured figures (§7) — so record it, but never infer a slowdown from it alone.
> The load was recorded **before and after every run** (§1) — mandatory here.

## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (UTC) | 2026-10-11 00:43–01:02 UTC |
| Host / kernel | Linux 7.2.9 (`#1-NixOS SMP PREEMPT_DYNAMIC`), `x86_64` |
| Distribution | NixOS 26.11 (Zokor) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 65 317 432 kB (62.29 GiB) |
| Host load (1-min `/proc/loadavg`) | cross-runtime `xrun` **434.54 → 415.14** (window 414–440); sys-only targets **3 → >600** (per-run table below) |
| Repository state | commit `844220c945` (the m6 measurement tree) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl` at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`, **OTP 29.1.1 / erts-17.1** (`otp_release` = `"29"`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, resolved from the Nix store by the driver's **store scan** (not on `PATH`); its shebang runs it under **Erlang/OTP 28** — see the honesty note on `runtimes[]` version reporting |
| `dotnet` | **not** in the store when the pass began (`plb` probed `dotnet-absent`); the .NET **8** and **9** SDKs were realized into the store with `nix build --no-link nixpkgs#dotnet-sdk` / `nixpkgs#dotnet-sdk_9`, after which the version-ranked store scan resolves **9.0.318** — which is what PLB's `.NET 9` target needs (§4.1) |
| `$BENCH_TRY_NIX` | **works** in this sandbox (verified: it fetches `rebar3` 3.27.1 + Erlang/OTP 28 from `cache.nixos.org`), but had **no effect** on any m6 run — the store scan resolved both tools locally first (the `bench list` probe output is identical with and without it) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` |

Both runtimes resolve and pass the harness self-check (`supported: true`).
The driver and the runtime were both built from this tree (`--release -p beam_rt -p bench`).

Per-run `/proc/loadavg` record (1-min value first; nothing rounded away):

| run | before | after |
|---|---|---|
| `bench list` probe diff | `8.44 3.99 5.94` | — |
| `khepri` | `131.72 39.14 17.74` | `637.80 277.96 107.48` |
| `emqx` | `602.98 276.72 107.99` | `559.90 273.20 107.76` |
| `plb` (SDK 8.0.425 → failed) | `559.90 273.20 107.76` | `526.28 270.99 107.93` |
| `plb` (SDK 9.0.318 → ok) | `475.17 337.95 155.09` | `490.82 411.25 217.89` |
| `cowboy` | `490.82 411.25 217.89` | `471.58 421.99 235.66` |
| `erlc_compiler` | `474.34 423.39 237.11` | `486.28 427.57 240.46` |
| `elixir_compiler` | `486.28 427.57 240.46` | `508.76 437.07 248.55` |
| `plb, khepri, emqx` (`BENCH_TRY_NIX=1`) | `503.33 437.13 249.59` | `429.84 432.30 281.54` |
| `xrun` (cross-runtime) — before build / before / after | `440.40 434.09 308.59` / `434.54 433.40 315.57` | `415.14 425.31 334.87` |
| `cargo test -p bench` | `427.63 431.74 282.97` | — |

## 2. Exact commands

Build the driver and the runtime:
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
```
Probe status (in the m6 store state every workload reports ready; the two-store outputs are byte-identical):
```sh
erts_rust/target/release/bench list --json
```
The **focused cross-runtime run** — all six `Runtimes::Both` workloads, `--full`, both runtimes → `results/m6-bench-xrun.json`:
```sh
erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m6-bench-xrun.json --verbose
```
Cross-runtime diff (archived as `results/m6-compare-cross-runtime.txt`; the tool diffs the **mean**):
```sh
erts_rust/target/release/bench compare \
    erts_rust/tools/bench/results/m6-bench-xrun.json \
    > erts_rust/tools/bench/results/m6-compare-cross-runtime.txt
```
The **sys-only target runs** (one report each; `BENCH_TRY_NIX=1` was set for the combined three-target run but is not required):
```sh
erts_rust/target/release/bench run --quick --runtime sys --workloads plb,khepri,emqx   --out-file erts_rust/tools/bench/results/m6-bench-tool-targets.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads plb               --out-file erts_rust/tools/bench/results/m6-bench-plb.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads khepri            --out-file erts_rust/tools/bench/results/m6-bench-khepri.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx              --out-file erts_rust/tools/bench/results/m6-bench-emqx.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads cowboy            --out-file erts_rust/tools/bench/results/m6-bench-cowboy.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads erlc_compiler      --out-file erts_rust/tools/bench/results/m6-bench-erlc-compiler.json --verbose
erts_rust/target/release/bench run --quick --runtime sys --workloads elixir_compiler    --out-file erts_rust/tools/bench/results/m6-bench-elixir-compiler.json --verbose
```
Realize the .NET SDKs the `plb` target needs (store state, not repository state):
```sh
nix build --no-link --print-out-paths nixpkgs#dotnet-sdk     # .NET 8 (8.0.425)
nix build --no-link --print-out-paths nixpkgs#dotnet-sdk_9   # .NET 9 (9.0.318) — what plb targets
```
Crate tests: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` — **213 + 4 + 1 + 3 passed, 0 failed** (including the real-store scan test that now sees both SDKs and still ranks the greatest version first).

Profile semantics: `--full` ⇒ `baseline` 50 startup samples / 5 compute samples, `slice` 25 timed runs/case, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200 files.
`--quick` ⇒ `baseline` 10 / 5, `slice` 5, `foreign`/`list_walk`/`copy_bound`/`gc_bound` 3, `cowboy` 8 connections × 2 s.
The per-runtime loop count `N` in the `foreign`/`list_walk`/`copy_bound`/`gc_bound` cases is **fixed per case** (not profile-dependent), so the `--full` sample counts only widen the estimate, never the loop length (§3.4–3.7).
**Run order is runtime-major**: all `sys` rows execute first, then all `rust` rows — under a *drifting* host load the two sides are measured in different windows (§6.1).

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

`sys` = OTP 29.1.1 with the JIT. `rust` = `beam_rt`, released: a working runtime whose per-process execution tier **defaults to the native baseline JIT** (`JitMode::Auto` ⇒ `Tier::BaselineJit` on the process's first entry, remembered across pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the only kill switch), with anything outside the modelled `genop.tab` surface returning a typed `unsupported`/`BeamError` rather than a wrong result. Its runtime path compiles **per function, lazily** on a function's first entry (`../../crates/beam_engine/src/jit/baseline/code.rs`) — never whole-module; the current tier and one-shot cold-start figures are in `../../crates/beam_engine/MEASURED.md` and `../../crates/beam_rt/PERF.md`, summarised in §3.9.
All tables in this section come from the **focused `xrun`** (`results/m6-bench-xrun.json`, `--full --runtime all`, load 434.54 → 415.14), which runs the six cross-runtime workloads back to back: **12/12 rows `ok`**.

### 3.1 Startup latency — `baseline` (n = 50, ms)

| stat | sys | rust | rust/sys |
|---|---|---|---|
| min | 121.11 | 17.01 | 0.14× |
| **mean** | **157.09** | **17.50** | **0.11× (rust 8.98× faster)** |
| p50 | 151.91 | 17.50 | 0.12× |
| p90 | 190.31 | 17.70 | 0.09× |
| **p99** | **250.33** | **17.92** | **0.07× (rust 13.97× faster)** |
| max | 283.89 | 18.07 | 0.06× |

Rust startup is both lower **and far flatter** (17.0–18.1 ms across 50 samples). This is not bare interpreter boot against a like start: the oracle is a 14-scheduler SMP VM whose boot loses much more to the contended host, while a `beam_rt` process is a small single-threaded binary that now resolves its code path **natively from the filesystem**, with no `erl` spawn (§6.2). The whole-process figures are therefore the trustworthy cross-runtime startup comparison (see also §7, where this same change moved rust startup 26× vs `m5`).

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `compute_loop_ms` p50 | 264.95 | 73.33 | 0.28× |
| `compute_ops_per_sec` | 3 548 370 | 13 588 957 | 0.26× (rust faster) |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 36 215 944 B (34.5 MiB) | **805 458 B (787 KiB)** | **44.96× lower** |
| peak RSS (`VmHWM`, whole workload) | 137 510 912 B (131.1 MiB) | 31 453 184 B (30.0 MiB) | 4.37× lower |

The loop is `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'` and runs under **both** runtimes.
**Caveat — read this loop as the native floor, not interpreter throughput.** It is dominated by the natively-served `lists:seq/2` + `lists:sum/1`; the execution-bound signal is §3.4–3.7.
`vm_memory_total_bytes` is a **separate** metric from the OS `peak_memory_bytes` (§6.5) and is never conflated with it: `beam_rt` serves `erlang:memory/0,1` through the `MemoryHook` seam, and the two runtimes are not reporting the same quantity (OTP's `total` includes allocator arenas and preloaded code; `beam_rt`'s is its much smaller in-process heap).

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

Every case printed the **same** stdout under both runtimes (report note: `add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5 base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, **mean** (ms); per-case peak RSS, **p50** (MiB, base 2):

| case | sys wall | rust wall | rust/sys wall | sys RSS | rust RSS | sys/rust RSS |
|---|---|---|---|---|---|---|
| `add` (fixture) | 146.62 | 17.95 | 0.12× | 66.3 | 14.3 | 4.63× |
| `fib` (fixture, n=20) | 228.73 | 17.92 | 0.08× | 66.0 | 14.3 | 4.61× |
| `pick` (fixture) | 190.36 | 18.87 | 0.10× | 66.2 | 14.7 | 4.49× |
| `calendar_last_day` | 176.22 | 21.64 | 0.12× | 66.5 | 15.4 | 4.31× |
| `queue_from_size` | 148.66 | 24.37 | 0.16× | 66.4 | 17.5 | 3.81× |
| `gb_trees_size` | 136.22 | 21.34 | 0.16× | 66.3 | 15.6 | 4.26× |
| `base64_encode_size` | 142.54 | 23.60 | 0.17× | 66.5 | 16.7 | 3.99× |
| `ordsets_from_size` | 182.12 | 19.96 | 0.11× | 66.3 | 15.2 | 4.37× |
| `string_str_length` | 153.37 | 36.90 | 0.24× | 66.3 | 25.9 | 2.56× |

Whole-workload peak RSS: sys **72 794 112 B (69.4 MiB)** / rust **27 447 296 B (26.2 MiB)** (2.65× lower).

**Read the wall column as *whole-process*, not *execution*.** Each case is a single call per process, so the wall time is dominated by process boot; six of the nine also call a *system* module. Per-case `rust` peak RSS **is** the like-for-like per-process memory comparison (one call per process, no loop accumulation): rust **14.3–25.9 MiB** vs sys **66.0–66.5 MiB** (2.56×–4.63× lower).
The `string_str_length` row is the family's **one-shot cold-start** case — the first entry pulls in a large Erlang module graph — and its current-state cost is §3.9.

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source* (`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system `erlc` and executed under both runtimes.
`ns/call` is the startup-cancelled marginal `(mean wall@N − mean wall@0) / N` (n = 10 samples each); `N` is per runtime so both marginals clear the timer noise. `wall_ms` is **not** comparable across runtimes (different `N`); `startup_ms` is.

| case (real function) | N sys / rust | sys ns/call | rust ns/call | rust/sys | startup p50 sys / rust (ms) | RSS p50 sys / rust (MiB) |
|---|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 1 500 000 / 20 000 | 423.66 | 31 729.88 | **74.89×** | 144.24 / 17.74 | 66.3 / 72.7 |
| `cowboy_bstr:capitalize_token/1` | 1 500 000 / 15 000 | 375.07 | 36 027.84 | **96.06×** | 125.48 / 17.80 | 66.4 / 75.7 |
| `emqx_base62:encode/1` | 3 000 000 / 25 000 | 216.54 | 27 184.74 | **125.54×** | 126.53 / 18.92 | 66.4 / 60.0 |
| `emqx_base62:decode/1` | 3 000 000 / 15 000 | 315.14 | 41 393.44 | **131.35×** | 131.21 / 22.84 | 66.8 / 62.1 |

All four cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout (`sys`: `bstr_to_lower=45000000 bstr_capitalize_token=45000000 emqx_base62_encode=48000000 emqx_base62_decode=33000000`; `rust`: `600000 / 450000 / 400000 / 165000` = `N × per_call`).

**This is the execution-bound result.** Startup is comparable while the *per-call* cost of a real foreign function is **~75×–131×** higher under `beam_rt`. The gap is a **real runtime cost, not a harness artifact**: the startup-cancelled marginal is stable and reproduces with a plain shell loop calling `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`. It is a `beam_rt` per-call host-context cost, not the engine's per-instruction dispatch (§3.8).
**`foreign` peak RSS is not a like-for-like memory comparison — see §6.7.**

### 3.5 `list_walk` — the per-element dispatch-throughput metric (n = 10/case)

A **BIF-free** body-recursive walk (`walk(N)` sums a 1000-element list `N` times; `build/2` makes the list once per process) runs under both runtimes; the reported figure is the startup-cancelled **ns per walked element**.

| runtime | `N` (walks) | elements walked | `ns_per_element` | `wall_ms` p50 | `startup_ms` p50 | peak RSS p50 |
|---|---|---|---|---|---|---|
| `sys` | 100 000 | 100 000 000 | **3.1312** | 393.39 | 140.93 | 66.4 MiB |
| `rust` | 1 000 | 1 000 000 | **340.5021** | 343.16 | 24.30 | 15.0 MiB |

`case_list_walk_elements` (the deterministic, contention-immune companion metric) is `1×10⁸` for sys and `1×10⁶` for rust; each run printed the deterministic constant `N × 500 500` (`rust`: `list_walk=N*500500=500500000`, `sys`: `…=50050000000`).
`ns/element` gives a **108.74×** rust/sys ratio, consistent with the §3.4 `foreign` per-call gap and the §3.6 `copy_bound` ratios: the same per-unit execution-throughput story, applied to pure dispatch.

### 3.6 `copy_bound` — the copy boundary: ETF, ETS and process send/receive (n = 10/case)

Three copy-boundary cases under both runtimes, each validating its own round-trip with a bound-variable pattern: an **ETF** `term_to_binary`/`binary_to_term` round trip, an **ETS** `ets:insert` + `ets:lookup` round trip, and a **two-process send/receive** copy. All three are startup-cancelled `ns/op`; the `ops` counter is deterministic.

| case | op | sys ns/op | rust ns/op | rust/sys | ops sys / rust |
|---|---|---|---|---|---|
| `etf_roundtrip` | `term_to_binary` / `binary_to_term` | 647.48 | 121 953.98 | **188.35×** | 1 500 000 / 5 000 |
| `ets_copy` | `ets:insert` + `ets:lookup` | 244.59 | 4 281.73 | **17.51×** | 2 000 000 / 100 000 |
| `send_recv` | two-process send/receive copy | 1 452.94 | 194 813.33 | **134.08×** | 500 000 / 2 000 |

All three ran under both runtimes (`3 ok, 0 unsupported`) with identical stdout (`sys`: `etf_roundtrip=1500000 ets_copy=2000000 send_recv=500000`; `rust`: `5000 / 100000 / 2000` = `N × 1`).
`ets_copy` is the cheapest boundary for both runtimes, and its rust/sys ratio (17.51×) is the narrowest in the whole suite — the other two (ETF encode/decode and the inter-process copy) are in the same ~130–188× band as the `foreign` per-call gap.
**`copy_bound` rust peak RSS is not like-for-like — the `ets_copy` row (N = 100 000) reaches 75.6 MiB vs sys 66.5 MiB (§6.7).**

### 3.7 `gc_bound` — allocate-and-abandon churn (n = 10/case)

`churn(N)` allocates and immediately abandons a 256-element list, a 256-byte binary and a 256-element tuple, `N` times; the reported figure is the startup-cancelled `ns/op`.

| runtime | `N` (churns) | `churn_ns_per_op` | `wall_ms` p50 | `startup_ms` p50 | peak RSS p50 |
|---|---|---|---|---|---|
| `sys` | 200 000 | **4 643.26** | 978.67 | 131.52 | 66.3 MiB |
| `rust` | 2 000 | **258 354.39** | 546.93 | 18.28 | 25.1 MiB |

`case_churn_ops` (deterministic) is `200 000` (sys) / `2 000` (rust); each run printed `churn=N*1` (matching its `N`).
The rust/sys ratio is **55.64×**. This row measures the allocate-and-abandon **wall time**; whether a runtime's collector actually runs during the loop is its own admission policy and is **not claimed here** (the report's own note says the same).

### 3.8 Reading the cross-runtime numbers

- **Startup / whole-process cost is the trustworthy cross-runtime comparison** (§3.1, §3.3): rust is 8.98× faster at `baseline` startup and 4.16×–12.77× faster per `slice` case, with 2.65× lower per-process peak RSS. A `beam_rt` process is small and starts in ~17–37 ms.
- **The startup-cancelled marginals are the per-unit execution-throughput signal** (§3.4–3.7), and they point the other way: rust costs **17.5×–188.4×** more per op/call/element. That residual is a `beam_rt` per-call runtime cost, not the engine's per-instruction dispatch (next bullet).
- **Only `slice` gives a like-for-like peak-RSS comparison** (§6.7). `foreign` / `list_walk` / `copy_bound` / `gc_bound` rust peak RSS grows with the per-runtime loop count `N`.
- The gap is a **`beam_rt` per-call host-context cost**, not `beam_engine` instruction dispatch (the dispatch arm is itself O(1) and allocation-free per instruction, and the baseline tier runs the common `gc_bif` arithmetic natively): the runtime-side costs behind it are `beam_rt` measurements owned by `../../crates/beam_rt/PERF.md` (the per-call dispatch section) and `../../crates/beam_rt/CONTEXT.md`, not `bench` output.
- **The `slice` whole-process column includes a one-shot cold start, and that cold start is bounded** (§3.9): the runtime path lowers a function on its first entry, so a process that enters a large Erlang module graph pays ~1.2× the interpreter tier rather than a whole-module compile.

### 3.9 One-shot cold start — the runtime compiles per function, lazily

Every `slice` case is a single call in a **fresh** `beam_rt` OS process (§3.3), so that wall column carries the cost of entering a system module for the first time. That cost is bounded by construction: the **runtime path** stages a **lazy** native artifact — one sealed mapping per function, lowered on that function's first entry of a `(func, pc)` (`../../crates/beam_engine/src/jit/baseline/code.rs`) — so a fresh process pays only for the functions it actually enters. The **eager** whole-module layout (`JitBackend::compile` / `CodeCache::get_or_compile`) exists for the whole-module introspection surface and for the `jit_bench` harness; it is **not** the runtime path.
A module-graph-heavy case such as `string_str_length` (`string:length/1` is ~7 995 IR instructions) therefore sits at **~1.2×** the interpreter tier:

| current-state figure | where it comes from |
|---|---|
| `bslice_string:str_length/1`, N = 30 interleaved samples — default (baseline JIT) **31.86–45.45 ms (min 31.86)** vs `BEAM_RT_TIER=interpreter` **25.51–40.48 ms (min 25.51)** ⇒ **~1.22–1.25×** | `../../crates/beam_engine/MEASURED.md` §"Measured (baseline-JIT cold start — lazy per-function compilation)" |
| the same shape through this driver — **31.97 ms** default / **31.92 ms** `BEAM_RT_TIER=interpreter` (min-of-3) | same section of `../../crates/beam_engine/MEASURED.md` |
| the tier defaults to the native baseline JIT for **every** process entry; `BEAM_RT_TIER=interpreter` is the only kill switch | `../../crates/beam_rt/PERF.md` §"What changed since `BEFORE`" and `../../crates/beam_rt/src/runner/tier.rs` |

**Caveat — `jit_bench` pins the hot path, not this path.** The engine's `jit_bench` micro-benchmark drives the **eager** whole-module artifact only, so its interpreter → baseline-JIT speed-ups (`../../crates/beam_engine/MEASURED.md` §"Measured (JIT tier micro-benchmark …)") describe the hot path with the compile amortised away, and say nothing about the lazy runtime path's per-entry compile cost.
The two cited documents are **current-state** records — re-run their own commands for the live figures rather than quoting this summary. And the host-load caveat applies to them exactly as it does here (§6.1): absolute latencies move with load, so read the *ratio* (default ÷ interpreter), and interleave the two modes, never a single sample.

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS** on the target projects.
The Rust runtime cannot be scheduled on any of them (`Runtimes::SysOnly`, §5), so there is no Rust column.
All absolute values in this section are inflated by the varied host load (§1) and are **not comparable across runs**; §7 states each move explicitly.

### 4.1 `plb` — Programming-Language-Benchmarks (dotnet)

`plb` **runs** at the default tool resolution. The driver's Nix-store scan ranks entries by the version named in the entry, so it resolves the **newest installed `dotnet` SDK, .NET 9.0.318** (the store also holds an 8.0.425 one), and the workload's `.NET 9` build stage succeeds — no `BENCH_TOOL_PATH` override and no `PATH` prefix are needed (§2). Two independent runs produced real numbers (`--quick`, 10 tests):

| metric | standalone run (`m6-bench-plb.json`) | combined run (`m6-bench-tool-targets.json`, `BENCH_TRY_NIX=1`) |
|---|---|---|
| build stage (`dotnet --task build`) | ok — 59 730 ms | ok — 22 881 ms |
| wall | 125 424.71 ms | 82 557.97 ms |
| `test_time_ms` (n = 10) min | 0.00 | 0.00 |
| `test_time_ms` p50 | 981.13 | 639.53 |
| `test_time_ms` mean | 1 474.44 | 1 327.90 |
| `test_time_ms` p99 | 4 779.91 | 3 723.84 |
| `test_time_ms` max | 4 860.88 | 3 756.98 |
| `test_mem_bytes` | 93 110 272 B (88.8 MiB) | 91 140 096 B (86.9 MiB) |
| peak RSS | 169 779 200 B (161.9 MiB) | 171 868 160 B (163.9 MiB) |

The per-test `test_time_ms` is the PLB tool's own measurement of its workload; the workload-level `wall_ms` also includes the build stage, which is the volatile half (the two runs differ ~35 % in wall time, mostly there). The two runs were taken at different host loads (§1).
**Before the .NET 9 SDK was realized, the same workload recorded `failed(plb-build-failed)`** — a first attempt resolved the SDK 8.0.425 store entry, and `dotnet --task build` died with `NETSDK1045` (`.NET SDK does not support targeting .NET 9.0`). That is recorded, not papered over; see the reproducibility note below.

### 4.2 `khepri` — Khepri `rebar3 eunit` (GC / data-structure bound)

`rebar3` 3.27.1 is resolved from the Nix store by the driver's **store scan** — no `PATH` prefix and no env-var opt-in are needed. It runs under **Erlang/OTP 28** (see the honesty note on `runtimes[]` version reporting).

| metric | standalone run (`m6-bench-khepri.json`) | combined run (`m6-bench-tool-targets.json`) |
|---|---|---|
| `rebar3 eunit` wall | 101 386.88 ms | 81 362.09 ms |
| peak RSS | 350 093 312 B (333.9 MiB) | 376 324 096 B (358.9 MiB) |

### 4.3 `emqx` — EMQX umbrella build (`make emqx-enterprise-compile`)

`emqx` **does not run**: `skipped(emqx-requires-otp-28)`, with no number fabricated. The blocker is a genuine toolchain constraint, verified first-hand:

* The checkout pins OTP 28 (`.tool-versions` → `erlang 28.5.0.7-2`; `env.sh` → `OTP_VSN=28.5.0.7-2`), but the host `erl` is `otp_release=29` / `symbolic version=17.1`.
* The mechanism, demonstrated directly on this host (exit `0`, **no output at all**): `erl -noshell -s init stop -eval 'io:format("ENV_MK_WOULD_BE_WRITTEN~n"), halt().'` — `init stop` halts before `-eval` is evaluated, so EMQX's dependency `:ezstd`'s makefile (which generates its `env.mk` with exactly this idiom) writes nothing, and EMQX's `mix compile` then aborts with `No rule to make target '…/env.mk'`.
* Swapping in a whole OTP-28 toolchain (the store `erl-28.5.0.7` cannot be paired with the OTP-29-built Elixir 1.20.4 / `mix` on `PATH`) is out of this crate's scope, as the workload's own module docs state. Nothing was changed to paper over it.

### 4.4 `cowboy` — hello_world HTTP (in-process HTTP/1.1 load generator, 8 connections × 2 s)

`copy_tree`s the checkout into a `ScratchDir`, `make`s `examples/hello_world` (erlang.mk git-fetches cowlib/ranch), starts the release and drives it with the in-process keep-alive loader.

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 66 938.67 ms |
| requests | 81 412 |
| **throughput** | **40 684.14 req/s** |
| errors | 0 |
| latency min / p50 / p90 | 0.024528 / 0.101487 / 0.348004 ms |
| **latency p99** | **1.628060 ms** |
| latency max | 7.669393 ms (n = 20 000) |
| peak RSS | 143 941 632 B (137.3 MiB) |

### 4.5 `erlc_compiler` — `erlc` over the stdlib + Elixir corpus slice

| metric | value |
|---|---|
| wall | 9 621.48 ms |
| files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| **throughput** | **6.236 files/s** (**4 833.25 LOC/s**) |
| peak RSS | 130 744 320 B (124.7 MiB) |
| notes | `corpus: 60 files from stdlib+elixir` |

The throughput divides the **attempted** file/LOC count (see the honesty note on `erlc_compiler`).

### 4.6 `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

Measures the **installed** `elixirc` (the in-repo checkout is unbuilt and read-only) over a staged `.ex` corpus.

| metric | value |
|---|---|
| wall | 21 709.84 ms |
| files attempted / `.beam` produced | 25 / **39** (24/25 compiled) |
| **throughput** | **1.796 files/s** (**1 114.38 LOC/s**) |
| compiler startup | 679.53 ms |
| peak RSS | 190 021 632 B (181.2 MiB) |
| notes | `batch compile failed; per-file fallback (version skew)`; `compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)` |

## 5. Workloads that did **not** run — verified reasons

| workload | status | verified reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | EMQX pins OTP 28 while the host `erl` is OTP 29; the `:ezstd` `env.mk` make-target's `erl -noshell -s init stop -eval …` idiom produces nothing under OTP 29, so `mix compile` aborts (§4.3). No number is fabricated. |
| `plb`, before the .NET 9 SDK was realized | `failed(plb-build-failed)` | The default scan resolved the SDK **8.0.425** store entry; `dotnet --task build` died with `NETSDK1045` (cannot target .NET 9.0). Left as measured; the current default resolution picks **9.0.318** and the workload runs (§4.1). |
| every `SysOnly` workload scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/bootstrap an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. (Not exercised as a row in m6 — the m6 sys-only runs used `--runtime sys` — but it is the harness behaviour.) |

No `SysOnly` workload fails on a *tool* in the m6 archive: `plb`, `khepri`, `cowboy`, `erlc_compiler` and `elixir_compiler` all run (§4).

## 6. Reproducibility, stability and known measurement caveats

### 6.1 The host is heavily loaded and the two runtimes are measured in different windows
`/proc/loadavg` 1-min was ≈ **414–440** for the cross-runtime run and **3 → >600** for the sys-only targets (§1). The runner is **runtime-major** (`sys` rows first, then `rust` rows), so under a drifting load the two sides see different conditions. Loadavg is host-wide and does **not** move monotonically with the measured figures (§7), so prefer ratios, prefer the startup-cancelled rows, and always record `/proc/loadavg` alongside an absolute figure. The `m4` primary run (load 4.77 → 11.59) remains the only near-quiet reference.

### 6.2 `beam_rt` resolves its code path natively from the filesystem
The runtime locates its system `ebin` directories (stdlib, kernel, erts) with a **native filesystem probe** (`crates/beam_rt/src/codeserver.rs`: `$ERTS_STDLIB_DIR` when set, else the standard OTP installation layout under the `erl` on `$PATH` — *resolved to a path, never executed* — then this executable's ancestors, `$ERL_TOP`, and standard prefixes). The system `erl` is only *run* as a last resort, for an installation that layout cannot see. This is why rust startup is now **~17–37 ms** per process rather than the ~230–460 ms of the `m5` run, and it is the single largest real change between `m5` and `m6` (§7). Setting `$ERTS_STDLIB_DIR` still short-circuits the probe; `bench` never sets it (the harness is a pure driver/observer and never mutates the runtime's environment beyond argv). The per-element/per-call metrics cancel whatever startup cost remains by construction.

### 6.3 `elixir_compiler` measures the installed `elixirc` 1.20.4
against the unbuilt, read-only 1.21.0-dev checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the artifact.

### 6.4 `cowboy` re-fetches cowlib/ranch in a fresh scratch copy on every run (network)
which is why its `build_ms` (~67 s) dominates the workload.

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
**Consequence:** the m6 rust `case_<name>_peak_rss_bytes` rows run at large `N` and are **not** like-for-like — e.g. `foreign` rust p50 is 60.0–75.7 MiB vs sys 66.3–66.8 MiB, and `copy_bound`'s `ets_copy` row reaches 75.6 MiB (N = 100 000) vs sys 66.5 MiB. Use `slice` (§3.3, one call per process, `N`=1) as the comparable per-process metric.

## 7. Deltas vs the previous runs (`m5`, `m4`)

Every figure below is quoted from the committed artifacts: `m6-*` (2026-10-11, cross-runtime load ≈414–440), `m5-*` (2026-10-09, load ≈90–153) and `m4-*` (2026-10-08, load 4.77 → 11.59). **The three runs sit at very different — and non-monotonic — host loads.**

**Cross-run comparability.** Only the **load-invariant** rows are safe to compare across runs: the startup-cancelled marginals and, above all, the **deterministic counters** (`*_ops`, `*_elements`). The absolute wall / startup / throughput rows are **not** cross-run comparable — they moved in *both* directions between `m6` and `m5` (several sys-only throughputs rose despite a 3–5× higher recorded loadavg), which is why `/proc/loadavg` is treated as a weak proxy throughout this document.

### Real, code-attributable changes

* **The rust runtime resolves its code path natively — the `erl` shell-out is gone.** Commit `da3eaeb4ce` (2026-10-10, between `m5` and `m6`) replaced the "spawn `erl` to find the stdlib" code-path resolution with the native filesystem probe of §6.2, keeping the `erl` shell-out only as a last resort. **Every rust per-process startup collapsed**, e.g. `baseline` rust startup p50 **454.40 ms (`m5`) → 17.50 ms (`m6`)** and `foreign` rust startup p50 234.3–248.4 ms → 17.7–22.8 ms — measured under a **higher** host load. This is the dominant real change between `m5` and `m6`, and the reason the m6 rust startup/wall figures are ~8–26× lower than both `m5` (loaded) and `m4` (quiet). It supersedes the former "`beam_rt` spawns `erl` once per process" caveat.
* **`plb` runs.** `m4`'s best case was `failed(plb-tool-failed)` (no number); the `m5` default resolution picked a .NET 8 SDK and only a forced `BENCH_TOOL_PATH` made PLB run. In `m6` the store genuinely holds a **.NET 9 SDK** (realized with `nix build nixpkgs#dotnet-sdk_9`), so the `.NET 9` build succeeds at the **default** resolution: `ok` with real metrics (§4.1).
* **Two new cross-runtime workloads, `copy_bound` and `gc_bound`** — added in commit `4dc6d47e7f`; the first workloads to exercise the **Msg/copy + Distrib/ETF + ETS** and **GC** surfaces under **both** runtimes (§3.6, §3.7). No `m5`/`m4` counterpart exists, so there is nothing to diff.

### Load-attributable / not comparable

* **`baseline` startup, `sys` side.** `m5` mean 307.08 ms → `m6` mean 157.09 ms — *lower* under a 4× higher loadavg. Host variation, not a code change (the rust side is the code-path change above).
* **Compiler throughput.** `erlc_compiler` 4.74 → **6.24** files/s and `elixir_compiler` 1.61 → **1.80** files/s. Both ran the system toolchain, unmodified; the `elixir_compiler` corpus size also differs (`m5` primary attempted 105 files, `m6` quick attempted 25), so only the rates are comparable and neither move is code-attributable.
* **Cowboy throughput.** `m5` 28 367.57 → `m6` 40 684.14 req/s, p99 3.8372 → 1.6281 ms. The in-process load generator and the server share one contended machine; the move is host variation.
* **`khepri` wall.** 98 436.55 → 101 386.88 ms — within host variation of one another.

### Unchanged

* **`baseline` `vm_memory_total_bytes` (rust)** — `m5` 801 294 B → `m6` 805 458 B (the two runtimes still report different quantities, §3.2).
* **`foreign` rust peak RSS still scales with `N`** (§6.7).
* **The `foreign` per-call gap is the stable per-unit execution-throughput story.** `m4` (quiet) 53.8×–72.0×; `m5` 53.1×–96.0× (`xrun`); `m6` **74.9×–131.4×** (`xrun`). The band widened again — the same per-unit cost, read at a different host load. `list_walk` tells the same story: `m5` rust **561.67** ns/element vs sys **8.43** (66.6×) → `m6` rust **340.50** vs sys **3.13** (108.74×).

### New this run

* **`copy_bound`** and **`gc_bound`** — two cross-runtime workloads (§3.6, §3.7).
* **`plb`** — now runs at the default tool resolution (§4.1).

## Honesty audit — measurement-integrity notes

No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always `failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`, `src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`/non-doc `unreachable!`, no `#[ignore]`. Remaining labelling wrinkles — none fabricates a value:

* **`tool::available()` returns true whenever the opt-in is active**, i.e. `$BENCH_TRY_NIX` is set to a non-empty value *and* `nix` is on `PATH` — regardless of whether the tool actually resolves (`src/workloads/tool.rs::available` → `resolve_tool_path(tool).is_some() || nix_opt_in()`). So `probe_ready` can read `true` while the `nix shell` shim later fails; `run()` still reports the honest skip/`failed` in that case, **never** a fabricated number.
* **On a `SysOnly` tool-driven row the report's `runtimes[]` version describes the driver's resolved oracle, not necessarily the Erlang the external tool ran.** The store `rebar3` 3.27.1 is an escript whose shebang is `#!/nix/store/…-erlang-28.5.0.7/bin/escript`, so `khepri`'s measured wall time is real but the row's `runtimes[].version` (`"29"`, the driver's `erl` on `PATH`) does not describe the OTP the suite actually ran under (§4.2). No metric is fabricated.
* **`foreign` maps an `erlc` compile failure to `skipped(sources-missing)`** (`src/workloads/foreign.rs:566`): when the sources *are* present but `erlc` fails, the row is a skip (the staging note carries the error), not `failed` — a real compile regression of the foreign sources can read as a benign skip. Contrast `list_walk`, which treats the identical shape as `failed(module-compile-failed)` (`src/workloads/list_walk.rs:509`), and `slice`, which maps a failed fixture rebuild to `skipped(fixture-missing)` (`src/workloads/slice.rs:560`).
* **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` divide the *attempted* file/LOC count** by the wall time (`src/workloads/erlc_compiler.rs:296,300`: `total_files`/`total_loc` from `files.len()`), not the `files_compiled` (produced `.beam`) count, so a partial `erlc` run overstates throughput relative to the `files_compiled` row beside it. `elixir_compiler` uses the produced count.
* **Stale in-file doc:** `src/workloads/baseline.rs` (module doc and the `MEMORY_EVAL` comment) still says `erlang:memory/0,1` "is not implemented by the Rust runtime yet"; `beam_rt` now serves it via the `MemoryHook` seam and the workload records a real `vm_memory_total_bytes` for `rust` (§3.2). `is_capability_gap` survives only as a fallback classifier.
* `proc.rs:126` (`h.join().unwrap_or_default()`) collapses a panicked stdout/stderr drain thread to an empty string; the drain threads have no realistic panic path, so this is latent, not active.

The `foreign`/`list_walk`/`copy_bound`/`gc_bound` per-call/per-element/per-op methodology is implemented exactly as stated (startup-cancelled `(mean wall@N − mean wall@0) / N`), and the registered-BIF `~116 ns/call` headline is **not** produced here — it is a `beam_rt` measurement (`../crates/beam_rt/CONTEXT.md`, `examples/perf_bif.rs`).

**Reproducibility note (PLB).** The .NET SDKs are *store* state, not repository state: a fresh sandbox that has not realized `nixpkgs#dotnet-sdk_9` will again report `plb` → `failed(plb-build-failed)` (with `NETSDK1045`) even though `probe()` reads ready. That is the honest, un-fabricated outcome for that environment; the committed artifacts above were produced with the 9.x SDK present. `khepri` needs only the `rebar3` store entry and runs as-is.

## Archived artifacts

| file | contents |
|---|---|
| `results/m6-bench-xrun.json` | **current cross-runtime run**: `--full --runtime all`, the six `Runtimes::Both` workloads × both runtimes, 12/12 rows `ok` (host load 434.54 → 415.14) — the source of §3 |
| `results/m6-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside `m6-bench-xrun.json` (69 numeric rows; the tool diffs the **mean**) |
| `results/m6-bench-plb.json` | PLB at the default tool resolution (`.NET 9.0.318`) → `ok` (§4.1) |
| `results/m6-bench-khepri.json` | Khepri `rebar3 eunit` standalone run (§4.2) |
| `results/m6-bench-emqx.json` | EMQX → `skipped(emqx-requires-otp-28)` (§4.3) |
| `results/m6-bench-cowboy.json` | Cowboy hello_world release + HTTP load (§4.4) |
| `results/m6-bench-erlc-compiler.json` | `erlc` over the stdlib+Elixir corpus (§4.5) |
| `results/m6-bench-elixir-compiler.json` | installed `elixirc` over the checkout's `.ex` corpus (§4.6) |
| `results/m6-bench-tool-targets.json` | `plb` + `khepri` + `emqx` in one report (`BENCH_TRY_NIX=1`) — the second PLB/khepri observations (§4.1, §4.2) |
| `results/m6-load-record.md` | the cross-runtime pass: exact commands, the `/proc/loadavg` before/after each run, and the per-workload sys-vs-rust ratios |
| `results/m6-tool-targets-notes.md` | the factual per-target status/blocker notes and the environment findings (store `rebar3`, realized .NET SDKs) |
| `results/m5-*` | the **previous** run (2026-10-09, load ~90–153): primary full run, focused `xrun`, the PLB runs, and the cross-runtime diff — regression baseline for §7 |
| `results/m4-*` | the run **before that** (2026-10-08, load 4.77 → 11.59): primary full run, the three xruns, the compiler/cowboy/khepri runs, the PLB attempts, and the `m4-compare-*.txt` diffs — the near-quiet reference |

The archived copies are the harness output except for two mechanical normalisations that strip environment-specific absolute paths: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` is replaced with `$TMPDIR/`. **No metric value is altered.**
