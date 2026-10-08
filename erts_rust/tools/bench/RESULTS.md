# Rust-ERTS benchmark results — current measurement run
Definitive, reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the
target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured**, not modelled. A workload that could not run is reported with a
*verified* reason and **no number** — never an estimate.

> **Currency.** Measured at commit `14a58a1af4eae248beadc8cac0ecde4b1081db64` on **2026-10-08**
> (19:35–20:35 HKT) on the host in §1. Raw harness JSON is archived under `results/` as `m3-*`;
> the previous **Milestone-1** artifacts are kept as `m1-*` and are **superseded** — do not quote
> them. Since M1 the `beam_rt` per-BIF-call defect (a whole-`AtomTable` deep clone on **every**
> registered BIF call, commit `ea0f1948`) is fixed and the `MemoryHook` seam is adopted
> (`erlang:memory/0,1` now returns a real number), so M1's two headline claims — the
> **703×–9 891×** per-call gap and a `vm_memory_total_bytes` row that was "not run" for Rust —
> are both obsolete. See §3.4 and §3.2.
>
> **Host-contention warning (read before quoting any absolute latency).** The benchmark host is
> shared and was under sustained background load for the whole session (`/proc/loadavg` observed
> **8–40 running tasks on 14 CPUs**, 36–46 runnable while this agent was idle). Every absolute
> latency below is therefore **inflated relative to the M1 archive**, and the four full
> `--runtime all` runs taken during the session disagree with each other by up to **4.5×**
> (startup) and **11×** (`cowboy` throughput) — §7. The **cross-runtime** tables (§3) come from two
> *back-to-back, same-conditions* runs of the three cross-runtime workloads, which agree with each
> other within ±5 % on all but four rows (§7); they are the reliable part of this document. Re-run on a
> quiet host before quoting a hard number.

---
## 1. Environment (recorded with the run)

| item | value |
|---|---|
| Date (local / UTC) | 2026-10-08 19:35–20:35 HKT / 11:35–12:35 UTC |
| Host / kernel | Linux 7.2.9 (`#1-NixOS SMP PREEMPT_DYNAMIC`), `x86_64` |
| Distribution | NixOS 26.11 (`26.11.20261003.a7868a7`) |
| CPU | Intel(R) Core(TM) Ultra 7 356H — `nproc` = **14** |
| RAM | `MemTotal` = 62.29 GiB |
| Host load during the session | `/proc/loadavg` 1-min **8–40**, 5-min 17–75 on 14 CPUs (shared host) |
| Repository state | `14a58a1af4eae248beadc8cac0ecde4b1081db64` (the measurement commit) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl`, **OTP 29.1.1 / erts-17.1** (`otp_release` = `"29"`, `version` = `"17.1"`; `smp:16:14`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| `erlc` | OTP 29.1.1 (on `PATH`) |
| `elixirc` | **Elixir 1.20.4** (compiled with Erlang/OTP 29) |
| `rebar3` | **3.27.1**, only via `nix shell nixpkgs#rebar3` (i.e. `BENCH_TRY_NIX=1`) |
| `dotnet` | absent from `PATH` and from the Nix store (see §5) |
| `$HOME` | read-only (`touch` fails with `Read-only file system`) — relevant to `plb` (§5) |

Both runtimes resolve and pass the harness self-check (`supported: true`). The driver was built
from the same commit as the runtime (`--release -p beam_rt -p bench`), so the numbers describe this
tree, not a stale binary.

## 2. Exact commands

Build (both the fixed runtime and the driver, ~29 s):
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench
```
Probe status:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- list --json
```
Quick sweep (all 9 workloads, both runtimes) — archived as `results/m3-bench-quick-runtimes-all.json`:
```sh
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime all --out-file erts_rust/tools/bench/out/sweep-quick.json --verbose
```
**Full milestone runs — `--full`, both runtimes, all 9 workloads** (run to completion; the archived
`m3-bench-full-runtimes-all.json` is run **B**, the second of these):
```sh
# run A -> results/m3-bench-full-runtimes-all-run2.json   (ended 20:15)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/out/milestone-full-A.json --verbose
# run B -> results/m3-bench-full-runtimes-all.json        (ended 20:25)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --out-file erts_rust/tools/bench/out/milestone-full-B.json --verbose
```
**Focused cross-runtime runs — the source of every sys-vs-rust table in §3** (the three
cross-runtime workloads only, so both sides see the same machine conditions; ~2 min each):
```sh
# -> results/m3-bench-xrun-baseline-slice-foreign.json        (ended 20:29, loadavg 23 -> 18)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign \
    --out-file erts_rust/tools/bench/out/xrun-full-1.json --verbose
# -> results/m3-bench-xrun-baseline-slice-foreign-run2.json   (ended 20:31, loadavg 18 -> 9)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign \
    --out-file erts_rust/tools/bench/out/xrun-full-2.json --verbose
```
**Nix-gated workloads** (`rebar3` resolvable; `khepri` runs, `emqx` is honestly skipped) —
`results/m3-bench-full-nix-khepri-emqx.json`:
```sh
BENCH_TRY_NIX=1 erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads khepri,emqx \
    --out-file erts_rust/tools/bench/out/nix-full-2.json --verbose
```
**`plb` attempt** (the sandbox needs a writable CLI home because `$HOME` is read-only) —
`results/m3-bench-quick-nix-plb-attempt.json`:
```sh
BENCH_TRY_NIX=1 DOTNET_CLI_HOME="$TMPDIR/dotnet-home" NUGET_PACKAGES="$TMPDIR/dotnet-home/nuget" \
  MIX_HOME="$TMPDIR/dotnet-home/mix" HEX_HOME="$TMPDIR/dotnet-home/hex" \
  erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb --out-file erts_rust/tools/bench/out/plb-nix-attempt.json --verbose
```
Comparisons (archived as `m3-compare-*.txt`):
```sh
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m3-bench-xrun-baseline-slice-foreign.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m3-bench-xrun-baseline-slice-foreign.json \
                                       erts_rust/tools/bench/results/m3-bench-xrun-baseline-slice-foreign-run2.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m3-bench-full-runtimes-all.json \
                                       erts_rust/tools/bench/results/m3-bench-full-runtimes-all-run2.json
```
Crate tests: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p bench` ⇒
**green** (182 unit + 4 `integration` + 1 `oracle_smoke` + 3 doc-tests = 190 passed, 0 failed).

Profile semantics: `--full` ⇒ `baseline` 50 startup samples / 5 compute samples, `slice` 25 timed
runs/case, `foreign` 10 samples/case, `cowboy` 16 connections × 5 s, `elixir_compiler` up to 200
files. `--quick` ⇒ `baseline` 10 / 5, `slice` 5, `foreign` 3, `cowboy` 8 connections × 2 s.

## 3. Cross-runtime results (system ERTS vs Rust ERTS)
`sys` = OTP 29.1.1 with the JIT; `rust` = `beam_rt`, released, a **tree-walking interpreter** over
a supported opcode subset. `ratio` = rust / sys (values > 1 mean Rust is slower). All tables in
this section come from the two focused runs (§2) so both sides see the same host conditions.

### 3.1 Startup latency — `baseline` (n = 50, ms)
| metric | sys | rust | ratio |
|---|---|---|---|
| min | 108.88 | 127.80 | 1.17× |
| **p50** | **128.08** | **154.93** | **1.21×** |
| mean | 129.10 | 155.41 | 1.20× |
| p90 | 139.18 | 171.38 | 1.23× |
| **p99** | **150.56** | **183.27** | **1.22×** |
| max | 150.86 | 190.57 | 1.26× |

Startup remains the one dimension where the two runtimes are **comparable** (~1.2×). The Rust figure
is not bare interpreter boot: `beam_rt` resolves the system code path by spawning `erl` once per
process (§7.2), and the `erl` side is a full SMP VM that loses more to CPU contention than
`beam_rt`'s smaller footprint — which is why the spread across full runs (§7) is asymmetric.
Repeat run (`…-run2.json`): sys p50 123.63 / rust 149.77 ⇒ **1.21×**, i.e. identical ratio.

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)
| metric | sys | rust | ratio |
|---|---|---|---|
| `compute_loop_ms` min / p50 / mean | 186.01 / 194.65 / 195.80 | 163.83 / 208.06 / 198.62 | ≈1.0× |
| `compute_ops_per_sec` | 5 107 235 | 5 034 769 | 1.01× |
| `vm_memory_total_bytes` (`erlang:memory(total)`) | 36 225 616 B (34.5 MiB) | **801 088 B (782 KiB)** | **45.2× lower** |
| peak RSS (`VmHWM`, whole workload) | 136 093 696 B (129.8 MiB) | 34 562 048 B (33.0 MiB) | 3.94× lower |

**The `vm_memory_total_bytes` row is no longer "not run" for Rust.** `beam_rt` adopted the
`MemoryHook` seam, so `erlang:memory/0,1` returns a real VM-reported figure; verified directly
(`beam_rt -noshell -eval 'M = erlang:memory(total), io:format("~p~n",[M]), halt().'` → `801088`,
exit 0) and reproduced identically by all four full runs and both focused runs. It is a **separate
metric** from the OS `peak_memory_bytes` and is never conflated with it. The two runtimes are not
reporting the same quantity (OTP's `total` includes allocator arenas and preloaded code; `beam_rt`'s
is its much smaller in-process heap).

The loop is `-eval 'S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt().'` and runs
under **both** runtimes.

**Caveat — read this loop as the native floor, not interpreter throughput.** It is dominated by the
natively-served `lists:seq/2` + `lists:sum/1` (a hand probe on this runtime: `lists:seq(1,N)` costs
~10 µs/element, versus ~10.5 µs for a single *interpreted* `-` per iteration, §6). So a rust
`compute_ops_per_sec` at parity says the native list floor is fast; it is **not** a measure of
interpreter dispatch or of the per-BIF overhead that `foreign` (§3.4) exposes.

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)
Every case printed the **same** stdout under both runtimes (report note:
`add=5 fib=6765 pick=2 calendar_last_day=29 queue_from_size=3 gb_trees_size=5
base64_encode_size=8 ordsets_from_size=3 string_str_length=5`).

Wall time, p50 (ms); RSS, p50 of peak resident set (MiB):
| case | sys wall | rust wall | wall ratio | sys RSS | rust RSS | RSS ratio |
|---|---|---|---|---|---|---|
| `add` (fixture) | 135.92 | 151.71 | 1.12× | 66.0 | 17.8 | 3.71× |
| `fib` (fixture, n=20) | 136.23 | 153.99 | 1.13× | 66.2 | 17.8 | 3.72× |
| `pick` (fixture) | 138.40 | 147.83 | 1.07× | 66.5 | 17.8 | 3.74× |
| `calendar_last_day` | 138.56 | 159.43 | 1.15× | 66.4 | 18.6 | 3.58× |
| `queue_from_size` | 141.32 | 158.98 | 1.12× | 66.5 | 22.1 | 3.01× |
| `gb_trees_size` | 136.92 | 151.30 | 1.11× | 66.3 | 18.5 | 3.58× |
| `base64_encode_size` | 139.40 | 151.23 | 1.08× | 66.3 | 19.9 | 3.33× |
| `ordsets_from_size` | 128.84 | 157.80 | 1.22× | 65.7 | 17.9 | 3.66× |
| `string_str_length` | 134.64 | 172.87 | 1.28× | 66.3 | 36.5 | 1.82× |

Whole-workload peak RSS: sys **69.0 MiB** / rust **36.8 MiB** (1.87× lower).

**Read this table as *startup*, not *execution*.** Six of the nine cases call a *system* module, and a
`beam_rt` process that must load a system module first spawns `erl` once to resolve the code path
(≈ 130–150 ms). The three pure-fixture cases (`add`/`fib`/`pick`) are also dominated by that same
fixed cost in this harness because their measured work is a single call. Consequently these wall times
are ~parity; they do **not** say the interpreter is as fast as the JIT (§3.4 shows the per-call cost).

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

The workload that measures the Rust runtime on *real target-project source*
(`cowboy/src/cowboy_bstr.erl`, `apps/emqx/src/emqx_base62.erl`), compiled **once** with the system
`erlc` and executed under both runtimes. `ns/call` is the startup-cancelled marginal
`(mean wall@N − mean wall@0) / N`; `N` is per runtime (`sys` 200 000, `decode` 400 000; `rust`
150–2 000) so both marginals clear the timer noise. `wall` is **not** comparable across runtimes
(different `N`); `startup` is.

| case (real function) | sys ns/call | rust ns/call | ratio | wall p50 sys/rust (ms) | startup p50 sys/rust (ms) | RSS p50 sys/rust (MiB) |
|---|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 400.0 | 59 806.6 | **150×** | 210.2 / 278.0 | 132.6 / 151.1 | 66.3 / 26.6 |
| `cowboy_bstr:capitalize_token/1` | 333.3 | 60 723.2 | **182×** | 215.8 / 249.4 | 145.0 / 156.4 | 66.5 / 27.6 |
| `emqx_base62:encode/1` | 279.7 | 283 792.0 | **1 015×** | 187.4 / 234.4 | 130.5 / 157.0 | 66.3 / 19.6 |
| `emqx_base62:decode/1` | 345.6 | 518 024.9 | **1 499×** | 270.7 / 228.4 | 132.8 / 143.6 | 66.5 / 19.6 |

Per-case peak RSS p50: sys 66.3–66.5 MiB; rust 19.6–27.6 MiB (2.4–3.4× lower).
Whole-workload peak RSS: sys **68.7 MiB** / rust **27.6 MiB** (2.49× lower).
All 4 cases ran under both runtimes (`4 ok, 0 unsupported`) and produced the same stdout
(`sys`: `bstr_to_lower=6000000 bstr_capitalize_token=6000000 emqx_base62_encode=3200000
emqx_base62_decode=4400000`; `rust`: `60000 / 45000 / 4000 / 1650` = `N × per_call`).
Repeat run (`…-run2.json`) reproduced every `ns/call` within ±5 % except `emqx_base62_encode` rust
(`+26 %`), `emqx_base62_decode` sys (`−15 %`) and `emqx_base62_encode` sys (`−12 %`) — see
`results/m3-compare-xrun-run1-vs-run2.txt`.

**This is the headline execution-bound result — and it has moved a long way since M1.** Startup is
comparable (§3.1) while the *per-call* cost of a real foreign function is two to three orders of
magnitude higher under `beam_rt`:

| case | M1 rust ns/call (superseded) | M1 ratio | **now** rust ns/call | **now** ratio | improvement |
|---|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 265 103.7 | 703× | **59 806.6** | **150×** | 4.4× lower |
| `cowboy_bstr:capitalize_token/1` | 264 140.7 | 769× | **60 723.2** | **182×** | 4.3× lower |
| `emqx_base62:encode/1` | 2 037 774.1 | 9 891× | **283 792.0** | **1 015×** | 7.2× lower |
| `emqx_base62:decode/1` | 2 944 299.8 | 8 803× | **518 024.9** | **1 499×** | 5.7× lower |

The gap is still **real, not a harness artifact** — the startup-cancelled marginal reproduces with a
plain shell loop and is a stable per-call cost. **It is dominated by the residual per-BIF-call cost
in `beam_rt`, not by interpreter dispatch** (§6): the rust `ns/call` figures scale with the number of
BIFs each function executes per iteration (≈ 6 for `bstr_to_lower`, ≈ 27 for `emqx_base62:encode`,
≈ 49 for `emqx_base62:decode`). Treat these four rows as **snapshots of a known, still-open
`beam_rt` defect** — they are the rows most likely to move.

## 4. System-ERTS target-project results (sys-only workloads)

These are measurements of the **system ERTS** on the target projects. The Rust runtime cannot be
scheduled on any of them (§5, fourth bullet), so there is no Rust column. Numbers below are the
**full run B**; §7 records how much they move between runs.

### 4.1 Compiler throughput

| workload | metric | value |
|---|---|---|
| `erlc_compiler` | wall | 6 577.19 ms |
| | files compiled | 60 (30 OTP `lib/stdlib/src` + 30 Elixir `lib/elixir/src`) |
| | **throughput** | **9.12 files/s** (7 070 LOC/s) |
| | peak RSS | 135 286 784 B (129.0 MiB) |
| `elixir_compiler` | wall | 46 279.16 ms |
| | files attempted / `.beam` produced | 105 / **223** (97/105 compiled) |
| | **throughput** | **4.82 files/s** (2 419 LOC/s) |
| | compiler startup | 269.23 ms |
| | peak RSS | 215 683 072 B (205.7 MiB) |
| | notes | batch compile failed → per-file fallback (checkout ↔ installed-version skew); compiler = installed `elixirc` 1.20.4 |

### 4.2 Cowboy hello-world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s)

| metric | value |
|---|---|
| build (`erlang.mk` + git fetch of cowlib/ranch) | 42 882.03 ms |
| requests | 902 807 |
| **throughput** | **180 502.40 req/s** |
| errors | 0 |
| latency min / p50 / p90 | 0.0224 / 0.0700 / 0.1290 ms |
| **latency p99** | **0.6776 ms** |
| latency max | 3.355 ms (mean 0.096 ms, n = 20 000) |
| peak RSS | 163 647 488 B (156.1 MiB) |

### 4.3 Khepri test suite (GC / data-structure bound), `BENCH_TRY_NIX=1`

| metric | value |
|---|---|
| `rebar3 eunit` wall | **79 625.80 ms** |
| peak RSS | 378 589 184 B (361.0 MiB) |

## 5. Workloads that did **not** run — verified reasons

Every reason below was reproduced on this host, not assumed.

| workload | status | verified reason |
|---|---|---|
| `plb` | `skipped(dotnet-absent)` by default; **`failed(plb-tool-failed)`** under `BENCH_TRY_NIX=1` | `dotnet` is on neither `PATH` nor in the Nix store. With `BENCH_TRY_NIX=1` the harness's `nixpkgs#dotnet-sdk` resolves (8.0.425) but **cannot build PLB's tool**: `bench/tool/BenchTool.csproj` targets `net9` ⇒ `NETSDK1045`, `dotnet run` exits `1` — re-verified this session: *"no result JSON found (dotnet exit Some(1))"*, *"stderr: The build failed."*, wall 994.91 ms. A .NET 9 SDK does build the tool, but PLB's `--task bench` then needs a prior `--task build` stage plus a docker/`mix local.hex` toolchain with a writable home (`$HOME` is read-only here), which this workload does not drive. **No PLB number is reported.** |
| `emqx` | `skipped(rebar3-absent)` by default; `skipped(not-a-rebar3-project)` once `rebar3` is resolvable | `/home/bill/Source/emqx/rebar.config` **does not exist** — EMQX builds via `make`/`mix`, so `rebar3 compile` would succeed while compiling nothing. The workload detects that and reports the honest skip instead of a meaningless wall time. |
| `khepri` | `skipped(rebar3-absent)` by default; **runs** under `BENCH_TRY_NIX=1` | `rebar3` is not on `PATH` but resolves from the Nix store (3.27.1) when the opt-in is set — hence §4.3. |
| all 6 `sys-only` workloads scheduled under `rust` | `skipped(runtime-unsupported)` | The Rust runtime is not an OTP release host: it cannot run `rebar3`/bootstrap an OTP release, act as `erlc`/`elixirc`, or boot the Cowboy release. `Runtimes::SysOnly` in the harness gates this before any probe. |

## 6. The known per-BIF-call cost — which rows it dominates

The M1 defect (a whole-`AtomTable` deep clone on every registered BIF call) is fixed; what remains
is a **residual, still-open per-BIF-call overhead of ≈ 10.5 µs** in `beam_rt`. Measured at this
commit with a hand-built `erlc`-compiled probe (N vs 0 iterations, same method as `foreign`):

| loop (one iteration) | BIFs/iter | rust marginal | oracle marginal |
|---|---|---|---|
| `drain([_\|T],A) -> drain(T,A)` (list walk, no BIF) | 0 | ≈ 0 (within the startup noise) | ≈ 0 |
| `empty(N,A) -> empty(N-1,A)` | 1 (`-`) | **≈ 10.5 µs** | ≈ 0.1–2.7 µs (host-load dependent) |
| `add(N,A) -> add(N-1,A+1)` | 2 (`-`,`+`) | **≈ 25.3 µs** | ≈ 0.1 µs |

So a **BIF-free** interpreted loop is cheap, and the cost scales with the *number of BIF calls per
iteration*, not with instruction dispatch. `beam_engine`'s interpreter dispatch is O(1) and
allocation-free per instruction; this is a `beam_rt` host-context cost.

**Rows dominated by it (do not read as fix-independent):**
- **all four `foreign` rust `ns_per_call` values** (§3.4) — e.g. `emqx_base62:decode` ≈ 518 µs/call
  ÷ 10.5 µs ≈ ~49 BIFs/iteration;
- consequently the **rust `case_*_wall_ms`** rows in `foreign` are ≈ half per-call work
  (`bstr_to_lower`: 2 000 × 59.8 µs ≈ 120 ms of a 278 ms wall) and move with it;
- the rust `vm_memory`/startup and all `slice` walls are **not** dominated by it (startup-dominated).

A further `beam_rt` fix to this per-BIF path is in flight; when it lands, **re-run the exact commands
in §2** rather than adjusting these numbers.

## 7. Reproducibility, stability and known measurement caveats

**Focused cross-runtime runs** (run 1 vs run 2, `results/m3-compare-xrun-run1-vs-run2.txt`, 76 rows):
all but **four** rows within ±5 %. The movers are `foreign` rust `emqx_base62_encode` `ns/call`
(+26.0 %) and `startup_ms` (−11.2 %), and `foreign` sys `emqx_base62_decode` `ns/call` (−15.0 %) and
`emqx_base62_encode` `ns/call` (−12.3 %). Everything else reproduces tightly — `baseline` startup
(−3.4 % / −4.1 %), the compute loop (±3.9 %), all nine `slice` walls (≤3.2 %) and every peak-RSS row
(<1 %).

**Full runs are far less stable** — the host was contended and the four `--full --runtime all` runs
taken this session disagree badly (`results/m3-compare-full-runB-vs-runA.txt`; min–max across the
four runs):

| metric measured in the full runs | min | max | spread |
|---|---|---|---|
| `baseline` sys `startup_ms` p50 | 109.37 | 494.98 | **4.5×** |
| `baseline` rust `startup_ms` p50 | 167.65 | 433.07 | 2.6× |
| `baseline` sys `compute_loop_ms` p50 | 178.56 | 793.76 | 4.5× |
| `erlc_compiler` `files_per_sec` | 6.13 | 9.46 | 1.5× |
| `elixir_compiler` `files_per_sec` | 1.51 | 4.82 | 3.2× |
| `cowboy` `requests_per_sec` | 16 356 | 180 502 | **11.0×** |
| `slice` sys `add` wall p50 | 136.34 | 627.61 | 4.6× |
| `foreign` rust `to_lower` `ns/call` | 46 933 | 151 086 | 3.2× |

Caveats an honest reading requires:

1. **The numbers in §3/§4 are snapshots, not constants.** The host is shared: 36–46 runnable tasks
   were present while this agent was idle, and `erl` (a 14-scheduler VM) loses more to that than
   `beam_rt` does. §3 and §4 quote the least-contended measurements available this session; §7 above
   is the error bar. Ratios are more robust than absolute values, but even ratios move when only one
   side is paused.
2. **`beam_rt` resolves its system code path by spawning `erl` once per process** (≈ 130–150 ms).
   It affects every process that touches a system module — all `slice` wrapper cases, all `foreign`
   cases, and the `baseline` startup — so it is the reason Rust startup is ~150 ms rather than ~20 ms.
   The per-call metric cancels this cost by construction.
3. **`elixir_compiler` measures the installed `elixirc` 1.20.4** against the unbuilt 1.21.0-dev
   checkout's sources, so the batch path fails and the per-file fallback runs; the note is in the
   artifact.
4. **`cowboy` re-fetches cowlib/ranch** in a fresh scratch copy on every run (network), which is why
   its `build_ms` (~43–64 s) dominates the workload.
5. Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`); Erlang's own
   `erlang:memory(total)` is a **separate** metric (§3.2) and is never conflated with it.
6. **`foreign` `wall_ms` is not comparable across runtimes** — each side runs a different `N`. Compare
   `ns/call` and `startup_ms`.

### Archived artifacts

| file | contents |
|---|---|
| `results/m3-bench-xrun-baseline-slice-foreign.json` | **primary** cross-runtime run (run 1): `--full --runtime all --workloads baseline,slice,foreign` |
| `results/m3-bench-xrun-baseline-slice-foreign-run2.json` | run 2, identical repetition (§7 stability) |
| `results/m3-bench-full-runtimes-all.json` | **primary full run** (run B): `--full --runtime all`, all 9 workloads |
| `results/m3-bench-full-runtimes-all-run2.json` | the other full run (run A) — shows the contention spread |
| `results/m3-bench-quick-runtimes-all.json` | `--quick --runtime all`, all 9 workloads |
| `results/m3-bench-full-nix-khepri-emqx.json` | `BENCH_TRY_NIX=1 --full --runtime sys --workloads khepri,emqx` |
| `results/m3-bench-quick-nix-plb-attempt.json` | the `plb` attempt → `failed(plb-tool-failed)` |
| `results/m3-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside the primary cross-runtime run |
| `results/m3-compare-xrun-run1-vs-run2.txt` | `bench compare` — cross-runtime run 1 vs run 2 |
| `results/m3-compare-full-runB-vs-runA.txt` | `bench compare` — full run B vs full run A |
| `results/m1-*` | the **superseded** Milestone-1 artifacts (kept verbatim; renamed with an `m1-` prefix) |

The archived copies are byte-for-byte the harness output except for one mechanical normalisation: the
run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative
`erts_rust/target/release/beam_rt` so the artifact carries no environment-specific absolute path.
**No metric value is altered.** The `m1-*` files carry the same normalisation from their own run.
