# Rust-ERTS benchmark results — milestone verification run

Definitive, reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the
target projects, produced by the `bench` driver (`erts_rust/tools/bench`) with the **`--full`
profile**. Raw machine-readable artifacts are archived under `results/` next to this file.

Everything below is **measured**, not modelled. A workload that could not run is reported with a
*verified* reason and **no number** — never an estimate.

---

## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (UTC) | 2026-10-07 23:59 → 2026-10-08 00:14 |
| Host / kernel | `billmini`, Linux 7.2.9 (`#1-NixOS SMP PREEMPT_DYNAMIC`), `x86_64` |
| Distribution | NixOS 26.11 (`26.11.20261003.a7868a7`) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 65 317 432 kB ≈ **62.3 GiB** |
| Repository state | `e7d90a7` (measurement runs), plus one follow-up change to `workloads/plb.rs` that only alters how a **failed** `plb` row is reported (see §5); every `plb` row in the archived full-run reports is `skipped`, so those reports are unaffected |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl`, **OTP 29.1.1 / erts-17.1** (`erlang:system_info(otp_release)` = `"29"`, `version` = `"17.1"`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, only via `nix shell nixpkgs#rebar3` (i.e. `BENCH_TRY_NIX=1`) |
| `dotnet` | absent from `PATH` and from the Nix store (see §5) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` (§5) |

Both runtimes resolve and pass the harness self-check (`supported: true`).

## 2. Profile and exact commands

Pre-built artifacts (`build`, ~19 s):

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p bench -p beam_rt
```

**Run 1 — the primary report** (`--full`, both runtimes, all 9 workloads; **239 s**):

```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/out/milestone-full.json --verbose
```

**Run 2 — identical repetition**, to quantify run-to-run stability (**233 s**):

```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/out/milestone-full-2.json
```

**Run 3 — the nix-gated workloads** (**78 s**):

```sh
BENCH_TRY_NIX=1 erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads khepri,emqx --out-file erts_rust/tools/bench/out/nix-full.json --verbose
```

**`plb` attempt** (quick; the sandbox needs a writable CLI home because `$HOME` is read-only):

```sh
BENCH_TRY_NIX=1 DOTNET_CLI_HOME="$TMPDIR/dotnet-home" NUGET_PACKAGES="$TMPDIR/dotnet-home/nuget" \
  MIX_HOME="$TMPDIR/dotnet-home/mix" HEX_HOME="$TMPDIR/dotnet-home/hex" \
  erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb --out-file erts_rust/tools/bench/out/plb-nix-full.json --verbose
```

Total measured wall time: **239 + 233 + 78 + 1 = 551 s** (≈ 9 min 11 s), plus the 19 s build.

Profile semantics: `--full` ⇒ `baseline` 50 startup samples, `slice` 25 timed runs/case, `foreign`
10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200 files. `cowboy`'s
in-load latency summary always collects 20 000 samples.

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

`sys` = OTP 29.1.1 with the JIT; `rust` = `beam_rt`, released, a **tree-walking interpreter** over
a supported opcode subset. `ratio` = rust / sys (values > 1 mean Rust is slower).

### 3.1 Startup latency — `baseline` (n = 50, ms)

| metric | sys | rust | ratio |
|---|---|---|---|
| min | 99.83 | 131.71 | 1.32× |
| mean | 118.44 | 151.03 | 1.28× |
| **p50** | **118.31** | **150.86** | **1.28×** |
| p90 | 130.42 | 164.65 | 1.26× |
| p99 | 145.39 | 172.29 | 1.19× |
| max | 148.06 | 172.40 | 1.16× |

Startup is the one dimension where the two runtimes are **comparable** (~1.3×). The Rust figure is
not bare interpreter boot: `beam_rt` resolves the system code path by spawning `erl` once per
process (see §6).

### 3.2 Fixed compute loop — `baseline` (n = 5)

| metric | sys | rust |
|---|---|---|
| `compute_loop_ms` p50 (sum of 1..1 000 000) | 190.48 | **not run** |
| `compute_loop_ms` mean | 183.24 | **not run** |
| `compute_ops_per_sec` | 5 457 387 | **not run** |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 55 294 449.6 | **not measured** |
| peak RSS (`VmHWM`) | 130 551 808 B (124.5 MiB) | `null` |

The Rust compute loop is **unimplemented for this expression**: all 5 attempts exited `1` with
`beam_rt: allocation failure: heap (16 bytes)`. The harness records that verbatim as a note and emits
**no** timing and **no** peak memory — there is no Rust number here to compare.

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

Every case printed the **same** stdout under both runtimes (report note:
`add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5
base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, p50 (ms); RSS, p50 of peak resident set (MiB):

| case | sys wall | rust wall | wall ratio | sys RSS | rust RSS | RSS ratio |
|---|---|---|---|---|---|---|
| `add` (fixture) | 132.68 | 149.33 | 1.13× | 66.1 | 17.6 | 3.8× |
| `fib` (fixture, n=20) | 134.98 | 156.71 | 1.16× | 66.3 | 17.6 | 3.8× |
| `pick` (fixture) | 126.40 | 148.45 | 1.17× | 66.0 | 17.5 | 3.8× |
| `calendar_last_day` | 133.76 | 148.36 | 1.11× | 66.3 | 18.3 | 3.6× |
| `queue_from_size` | 133.32 | 156.96 | 1.18× | 66.3 | 21.9 | 3.0× |
| `gb_trees_size` | 133.41 | 153.96 | 1.15× | 66.4 | 18.6 | 3.6× |
| `base64_encode_size` | 134.30 | 152.02 | 1.13× | 66.2 | 19.6 | 3.4× |
| `ordsets_from_size` | 135.51 | 152.18 | 1.12× | 66.4 | 17.9 | 3.7× |
| `string_str_length` | 129.40 | 170.27 | 1.32× | 66.2 | 36.3 | 1.8× |

Whole-workload peak RSS: sys **68.32 MiB** / rust **36.65 MiB** (1.9× lower).

**Read this table as *startup*, not *execution*.** Six of the nine cases call a *system* module, and a
`beam_rt` process that must load a system module first spawns `erl` once to resolve the code path
(≈ 110–140 ms). The three pure-fixture cases (`add`/`fib`/`pick`) are also dominated by that same
fixed cost in this harness because their measured work is a single call. Consequently these wall times
are ~parity; they do **not** say the interpreter is as fast as the JIT (§3.4 shows the per-call cost).

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source*
(`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system
`erlc` and executed under both runtimes. `ns/call` is the startup-cancelled marginal
`(mean wall@N − mean wall@0) / N`; `N` is per runtime (`sys` 200 000, `decode` 400 000; `rust`
150–2 000) so both marginals clear the timer noise.

| case (real function) | sys ns/call | rust ns/call | ratio | wall p50 sys/rust (ms) | startup p50 sys/rust (ms) |
|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 377.12 | 265 103.74 | **703×** | 204.42 / 682.73 | 131.81 / 152.27 |
| `cowboy_bstr:capitalize_token/1` | 343.54 | 264 140.68 | **769×** | 200.56 / 540.76 | 132.23 / 146.05 |
| `emqx_base62:encode/1` | 206.03 | 2 037 774.10 | **9 891×** | 176.31 / 658.59 | 134.14 / 145.97 |
| `emqx_base62:decode/1` | 334.46 | 2 944 299.75 | **8 803×** | 266.06 / 596.86 | 130.59 / 150.90 |

Per-case peak RSS p50: sys 66.2–66.3 MiB; rust 19.8–27.6 MiB (2.4–3.4× lower).
Whole-workload peak RSS: sys **68.66 MiB** / rust **27.79 MiB** (2.5× lower).
All 4 cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout
(`sys`: `bstr_to_lower=6000000 bstr_capitalize_token=6000000 emqx_base62_encode=3200000
emqx_base62_decode=4400000`; `rust`: `60000 / 45000 / 4000 / 1650` = `N × per_call`).

**This is the headline execution-bound result**: startup is comparable (§3.1) while the *per-call*
cost of a real foreign function is three to four orders of magnitude higher under `beam_rt` —
because the Rust runtime interprets, and the oracle JIT-compiles (`:jit:ns`).

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS** on the target projects. The Rust runtime cannot be
scheduled on any of them (§5, fourth bullet), so there is no Rust column.

### 4.1 Compiler throughput

| workload | metric | value |
|---|---|---|
| `erlc_compiler` | wall | 6 346.77 ms |
| | files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| | **throughput** | **9.45 files/s** (7 327 LOC/s) |
| | peak RSS | 134 873 088 B (128.6 MiB) |
| `elixir_compiler` | wall | 45 926.22 ms |
| | files attempted / `.beam` produced | 105 / 223 |
| | **throughput** | **4.86 files/s** (2 438 LOC/s) |
| | compiler startup | 276.87 ms |
| | peak RSS | 212 324 352 B (202.5 MiB) |
| | notes | batch compile failed → per-file fallback (checkout ↔ installed-version skew); compiler = installed `elixirc` 1.20.4 |

### 4.2 Cowboy hello-world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s)

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 38 880.99 ms |
| requests | 1 094 402 |
| **throughput** | **218 788.02 req/s** |
| errors | 0 |
| latency p50 | 0.0628 ms |
| latency p90 | 0.1009 ms |
| **latency p99** | **0.3868 ms** |
| latency max | 3.108 ms (mean 0.0773 ms, n = 20 000) |
| peak RSS | 160 841 728 B (153.4 MiB) |

### 4.3 Khepri test suite (GC / data-structure bound), `BENCH_TRY_NIX=1`

| metric | value |
|---|---|
| `rebar3 eunit` wall | **76 903.77 ms** |
| peak RSS | 365 961 216 B (349.0 MiB) |

## 5. Workloads that did **not** run — verified reasons

Every reason below was reproduced on this host, not assumed.

| workload | status | verified reason |
|---|---|---|
| `plb` | `skipped(dotnet-absent)` (default probes) / `failed(plb-tool-failed)` under `BENCH_TRY_NIX=1` | `dotnet` is on neither `PATH` nor in the Nix store (0 built outputs). With `BENCH_TRY_NIX=1` the harness's `nixpkgs#dotnet-sdk` *does* resolve (8.0.425, 20 s from `cache.nixos.org`) — but it **cannot build PLB's tool**: `bench/tool/BenchTool.csproj` declares `<TargetFramework>net9</TargetFramework>` → `error NETSDK1045: The current .NET SDK does not support targeting .NET 9.0`, `dotnet run` exits `1` ("The build failed."). A .NET 9 SDK (`nixpkgs#dotnet-sdk_9`, 9.0.318) *does* build the tool, but PLB's `--task bench` then requires a prior `--task build` stage plus a docker/`mix local.hex` toolchain with a writable home (`$HOME` is read-only in this sandbox), which the `plb` workload does not drive. **No PLB number is reported.** |
| `emqx` | `skipped(rebar3-absent)` by default; `skipped(not-a-rebar3-project)` once `rebar3` is resolvable | `/home/bill/Source/emqx/rebar.config` **does not exist** — EMQX builds via `make`/`mix`, so `rebar3 compile` would succeed while compiling nothing. The workload detects that and reports the honest skip instead of a meaningless wall time. |
| `khepri` | `skipped(rebar3-absent)` by default; **runs** under `BENCH_TRY_NIX=1` | `rebar3` is not on `PATH` but resolves from the Nix store (3.27.1) when the opt-in is set — hence §4.3. |
| all 6 `sys-only` workloads scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/`bootstrap` an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. |

`plb`'s honesty note (a change made in this task): a tool run that produces no result JSON is now
recorded `failed` (`plb-tool-failed` if the tool exited non-zero, `plb-no-results` if it exited `0`
having benched nothing) — previously such a row was reported `ok` with only the tool's own wall time,
which is not a benchmark result.

## 6. What these numbers do and do not mean

**They do mean:**

- On this host, with the `--full` profile, the **system-ERTS baseline and the target-project
  workloads above behave as reported**; the archived JSON can be re-diffed with
  `bench compare results/bench-full-runtimes-all.json`.
- **Startup latency is comparable** between the two runtimes (rust ≈ 1.28× sys, p50 150.9 vs
  118.3 ms), and the Rust runtime's **peak RSS is 1.8–2.5× lower** on the same programs.
- **Execution-bound work is not comparable yet.** For real foreign functions the Rust runtime costs
  **703×–9 891×** more per call than the JITed oracle. That is the expected shape of a tree-walking
  interpreter versus BeamAsm, and it is the gap the JIT tiers (Stage 1) exist to close.
- The nine `slice` programs and the four `foreign` cases agree with the oracle on **observable
  output** under both runtimes.

**They do not mean:**

- **Not a general statement about BEAM vs Rust performance.** The two sides are not equivalent
  implementations: `sys` is a full OTP release with a JIT and preloaded runtime; `rust` is a
  supported-subset interpreter. Ratios here measure *maturity*, not language speed.
- **Not a claim that the interpreter is at parity on `slice`.** Those wall times are dominated by the
  fixed `beam_rt` code-path resolution (§7) and by one-call programs; §3.4 is the honest per-call
  measurement.
- **Not milestone/absolute figures.** Single host, `--full` but finite sample counts, machine under
  normal container load. Reproduce on your host before quoting.
- **Not complete coverage.** `compute_loop_ms` under Rust does not exist (the expression is
  unimplemented); `plb`, `emqx` and the Rust side of all repo/tool workloads have no numbers.
- **Not fabricated anywhere.** Every skipped/failed row carries a reason and no metric; the
  `plb` row is `failed` rather than `ok` precisely so its wall time cannot be misread as a result.

## 7. Reproducibility, stability and known measurement caveats

Run 1 vs an identical Run 2 (`results/compare-run1-vs-run2.txt`, 88 numeric rows):

| metric | run 1 | run 2 | Δ |
|---|---|---|---|
| `baseline` sys `startup_ms` mean | 118.44 | 114.83 | −3.0 % |
| `baseline` rust `startup_ms` mean | 151.03 | 148.45 | −1.7 % |
| `baseline` sys `compute_ops_per_sec` | 5 457 387 | 6 184 379 | +13.3 % |
| `cowboy` `requests_per_sec` | 218 788 | 209 168 | −4.4 % |
| `erlc_compiler` `files_per_sec` | 9.45 | 9.85 | +4.2 % |
| `elixir_compiler` `files_per_sec` | 4.86 | 4.78 | −1.7 % |
| `slice` sys walls | — | — | all within ±6.1 % |
| `slice` rust walls | — | — | 8 of 9 within ±4.7 % |
| `slice` rust `string_str_length` | 172.45 | 255.33 | **+48.1 %** |
| `foreign` sys `ns_per_call` | — | — | −1.9 % … +15.7 % |
| `foreign` rust `ns_per_call` | — | — | +3.7 %, +3.9 %, +11.0 %, **+35.5 %** (`bstr_to_lower`) |

Caveats an honest reading requires:

1. **Per-call metrics are the noisiest.** `foreign`'s rust `ns/call` moved up to **+35.5 %** between
   identical runs; treat the three-to-four-order-of-magnitude gaps as robust, the two-digit
   percentages as not.
2. **`beam_rt` resolves its system code path by spawning `erl` once per process** (≈ 110–140 ms).
   It affects every process that touches a system module — all `slice` wrapper cases, all `foreign`
   cases, and the `baseline` startup — so it is the reason Rust startup is ~150 ms rather than ~20 ms.
   Measured directly on this host: `beam_rt -noshell -eval 'halt().'` takes **139–156 ms** (5 runs)
   bare and **13–20 ms** (5 runs) with `ERTS_STDLIB_DIR=<code:lib_dir(stdlib)>` pre-set. The harness
   deliberately does **not** set it, being a pure driver/observer. The per-call metric cancels this
   cost by construction.
3. **`elixir_compiler` measures the installed `elixirc` 1.20.4** against the unbuilt 1.21.0-dev
   checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the
   artifact.
4. **`cowboy` re-fetches cowlib/ranch** in a fresh scratch copy on every run (network), which is why
   its `build_ms` (~39–41 s) dominates the workload.
5. Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`); Erlang's own
   `erlang:memory(total)` is a **separate** metric and is never conflated with it.

### Archived artifacts

| file | contents |
|---|---|
| `results/bench-full-runtimes-all.json` | **primary** — Run 1: `--full --runtime all`, all 9 workloads |
| `results/bench-full-runtimes-all-run2.json` | Run 2: identical repetition (stability) |
| `results/bench-full-nix-khepri-emqx.json` | Run 3: `BENCH_TRY_NIX=1`, `--full --runtime sys`, `khepri,emqx` |
| `results/bench-quick-nix-plb-attempt.json` | the `plb` attempt → `failed(plb-tool-failed)`, verified reason |
| `results/compare-cross-runtime.txt` | `bench compare` — sys vs rust inside Run 1 |
| `results/compare-run1-vs-run2.txt` | `bench compare` — Run 1 vs Run 2 (88 numeric rows) |

The archived copies are byte-for-byte the harness output except for one mechanical normalisation:
the run-host worktree prefix in `runtimes[].program` is replaced with `erts_rust/target/release/beam_rt`
so the artifact carries no environment-specific absolute path. **No metric value is altered.**
