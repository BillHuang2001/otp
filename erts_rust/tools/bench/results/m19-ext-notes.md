# m19 stage 2 — external targets (3 reps), `--full` startup pass, latency-budget verdict

Session 2026-10-23/24, same host as stage 1: `erl` = **OTP 29.1.1 / erts-17.1**
(`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1`), `dotnet` **9.0.318** on `PATH`,
`rebar3` **absent from `PATH`** but resolved from the Nix store by the shared store scan
(`rebar3-3.27.3`), network available (Cowboy's erlang.mk git-fetches cowlib+ranch).

This file is **stage 2**: the whole-program targets, re-measured three times each, plus the
milestone-profile startup pass and the complete latency-budget verdict block.
Stage 1 — the six matched cross-runtime per-unit rounds — is *not* re-run and not repeated here; it
lives in `results/m19-xab-{rust,sys}-r{1..6}.json` + `results/m19-xab-ratios.txt` +
`results/m19-xab-load.log`, and its activation evidence is `results/m19-target-notes.md`.

**Every target in §1 is `Runtimes::SysOnly`: there is no `rust` row for any of them.** The Rust
runtime cannot boot an OTP release, run `rebar3`/`mix`, or act as `erlc`/`elixirc`/a Cowboy release
host, so each `--runtime sys` report carries exactly **one** row. These are *whole-program* figures
for the **target's own toolchain** — not rust-vs-sys comparisons. (The only two-sided rows in m19 are
stage 1's nine workloads and the `--full` `baseline` pass in §3.)

No metric value was edited anywhere; the only changes applied to the archived JSON are the mechanical
path normalisations (worktree prefix → `erts_rust/target/release/beam_rt`; the `/tmp/…` scratch root →
`$TMPDIR/`; the build-run worktree prefix stripped from the `gc_class` compile-source note).

## 0. Run protocol

* **One process at a time, foreground**, never two benchmarks concurrently; each run is its own
  command with a long timeout.
* `/proc/loadavg` recorded **before and after every pass** (timestamp + label + 1-min value) into
  `results/m19-ext-load.log`: **40 readings over 20 passes**, 1-min load **0.71 – 3.73** (a much
  quieter window than m17's 3–92). Absolute wall times remain host-load-sensitive; the deterministic
  counters (`requests`, `files_compiled`, the eunit test count, PLB's kernel list) are not.
* The two `--full` `baseline` passes and everything in stage 1 are pinned with `taskset -c 3`; the
  external targets are **unpinned**, so they stay comparable with m16/m17's external rows.
* The `bench` binary is the release build of this tree
  (`erts_rust/scripts/cargo.sh build --release -p beam_rt -p bench --manifest-path erts_rust/Cargo.toml
  --offline`).

## 1. The five targets — 3 reps each, and the median

All 15 runs returned `status: ok`; **no rep was `skipped` and no rep `failed`**, so every median below
is the median of three real measurements.

| target | metric | r1 | r2 | r3 | **median** |
|---|---|---|---|---|---|
| `erlc_compiler` | `files_per_sec` | 9.842 | 9.831 | 9.704 | **9.831 files/s** |
| | `loc_per_sec` | 7 627.7 | 7 619.8 | 7 521.0 | **7 619.8 loc/s** |
| | `wall_ms` | 6 096.6 | 6 102.9 | 6 183.1 | **6 102.9 ms** |
| | `peak_memory_bytes` (VmHWM) | 130 965 504 | 131 629 056 | 134 402 048 | **131 629 056 B** |
| `elixir_compiler` | `files_per_sec` | 3.463 | 3.414 | 3.405 | **3.414 files/s** |
| | `loc_per_sec` | 2 148.4 | 2 118.1 | 2 112.5 | **2 118.1 loc/s** |
| | `compiler_startup_ms` | 277.07 | 319.05 | 289.52 | **289.52 ms** |
| | `wall_ms` | 11 260.7 | 11 422.2 | 11 452.3 | **11 422.2 ms** |
| | `peak_memory_bytes` | 204 984 320 | 205 135 872 | 197 943 296 | **204 984 320 B** |
| `cowboy` | `requests_per_sec` | 124 029.7 | 118 779.4 | 120 383.9 | **120 383.9 req/s** |
| | `requests` | 248 207 | 237 789 | 240 926 | **240 926** |
| | `latency_ms` p50 | 0.0634 | 0.0633 | 0.0629 | **0.0633 ms** |
| | `latency_ms` p99 | 0.1081 | 0.1452 | 0.1553 | **0.1452 ms** |
| | `latency_ms` max | 1.099 | 1.379 | 2.901 | **1.379 ms** |
| | `build_ms` | 41 177.0 | 42 368.1 | 41 764.3 | **41 764.3 ms** |
| | `errors` | 0 | 0 | 0 | **0** |
| | `peak_memory_bytes` | 148 336 640 | 157 597 696 | 155 131 904 | **155 131 904 B** |
| `khepri` | `wall_ms` | 83 596.0 | 93 422.9 | 83 530.3 | **83 596.0 ms** |
| | eunit tests / failures | 1501 / 0 | 1501 / 0 | 1501 / 0 | **1501 / 0** |
| | suite-internal `Finished in` | 74.986 s | 84.620 s | 74.648 s | **74.986 s** |
| | `peak_memory_bytes` | 363 696 128 | 366 804 992 | 364 507 136 | **364 507 136 B** |
| `plb` | `wall_ms` | 68 084.4 | 68 297.5 | 68 266.1 | **68 266.1 ms** |
| | build stage (`dotnet --task build`) | 26 116 | 22 920 | 21 499 | **22 920 ms** |
| | `test_time_ms` count | 8 | 8 | 8 | **8 kernels** |
| | `test_time_ms` min | 181.42 | 187.19 | 192.01 | **187.19 ms** |
| | `test_time_ms` mean | 1 461.59 | 1 471.89 | 1 469.70 | **1 469.70 ms** |
| | `test_time_ms` p50 | 903.28 | 945.66 | 929.15 | **929.15 ms** |
| | `test_time_ms` max | 3 413.99 | 3 422.05 | 3 407.35 | **3 413.99 ms** |
| | `test_mem_bytes` | 91 496 448 | 92 479 488 | 93 372 416 | **92 479 488 B** |
| | `peak_memory_bytes` | 167 841 792 | 171 728 896 | 171 618 304 | **171 618 304 B** |

Rep-to-rep spread is small for every target (≤ ~12 % on `khepri`'s wall time, ≤ ~5 % elsewhere) — the
1-min load stayed in a 0.7–3.7 band throughout.

### 1.1 `erlc_compiler` — system `erlc` over OTP stdlib + Elixir `lib/elixir/src`
Deterministic shape in all three reps: **60 files / 46 503 LOC** compiled
(`files_compiled = 60`, `files_attempted = 60`). Row notes:
`code:which(compile)=…/lib/compiler-10.0.6/ebin/compile.beam (the system OTP installation)` — this is
the **system** compiler, never the in-tree `bootstrap/` one. **No rust row (`SysOnly`).**

### 1.2 `elixir_compiler` — installed system `elixirc` over a staged `.ex` corpus
**Mode: per-file fallback, not one batch.** All three reps carried the note
`batch compile failed; per-file fallback (version skew)`, `24/25 compiled; 39 .beam produced`
(`files_attempted = 25`, `files_compiled = 39` — an Elixir source may define several modules).
Compiler identity: `Elixir 1.20.4 (compiled with Erlang/OTP 29)`; the corpus is the **unbuilt,
read-only checkout** (`1.21.0-dev`) staged into `$TMPDIR`, so the skew is expected and is why the
batch path is not usable. **No rust row (`SysOnly`)** — nothing here runs a compiler *under* `beam_rt`.

### 1.3 `cowboy` — real `hello_world` relx release, in-process keep-alive load
Each rep `copy_tree`s the checkout into a scratch `$TMPDIR` copy, `make`s `examples/hello_world`
(erlang.mk **git-fetches cowlib + ranch — network**), starts the release `foreground` on port 8080 and
drives it with 16 keep-alive HTTP/1.1 connections. Note: **no notes were emitted on any rep** (clean
build, clean run). Throughput is bounded by the driver's fixed concurrency (`16 / per-request
latency`), so read `req/s` together with the `latency_ms` summary: p50 ≈ 0.063 ms and p99 0.11–0.16 ms
here, an order of magnitude tighter than m17's contended pass (p99 3.03–3.59 ms, ~38 k req/s) and in
line with m19's earlier activation check (~242 k requests, 121 k req/s, p99 0.127 ms). **No rust row
(`SysOnly`)** — the Rust runtime cannot boot an OTP release.

### 1.4 `khepri` — `rebar3 eunit` in a scratch copy
All three reps: **`1501 tests, 0 failures`** (`Finished in` 74.986 / 84.620 / 74.648 s). `rebar3`
resolved by the store scan (no `PATH` prefix, no `$BENCH_TRY_NIX`) at
`/nix/store/hmvrwzxq25f7ps0lnm62d6qqhiapfdwp-rebar3-3.27.3/bin/rebar3`.
**Which Erlang actually ran the suite:** the row's note names it —
`an escript whose shebang runs /nix/store/y5db8dvib9fr41l53qrhgzpgx6sbb04b-erlang-29.1.2/bin/escript`,
i.e. **OTP 29.1.2**, which is close to but *not* identical with the report's `runtimes[]` row (the
driver's `erl` on `PATH`, OTP 29.1.1). The measured wall time is real; the runtime version on this row
describes the driver's oracle, not necessarily the Erlang the external tool chose. **No rust row
(`SysOnly`)**.

### 1.5 `plb` — Programming-Language-Benchmarks via the `dotnet` tool
Two stages per rep, exactly as documented: `--task build --langs elixir --no-docker`, then
`--task bench --langs elixir`; results read from the **scratch** copy's `bench/build/_results/**`.
`dotnet` provenance in every rep's notes:
`resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)`, with **both** 8.0.425 store
candidates named as rejected (`rejected dotnet 8.0.425 … : requires >= 9`).
**Excluded (untimed) kernels — identical in all three reps:** the tool reported
`timeMS = 0` for **`spectral-norm/4000` and `spectral-norm/8000`** (its marker for a run it could not
time — aborted at the problem's declared timeout); they are excluded from `test_time_ms` and named in
a note. The **8** timed kernels (r1 values): `coro-prime-sieve/1000=363 ms`,
`coro-prime-sieve/4000=2553 ms`, `edigits/100000=687 ms`, `edigits/250001=2937 ms`,
`helloworld/QwQ=181 ms`, `pidigits/4000=439 ms`, `pidigits/8000=1119 ms`,
`spectral-norm/2000=3414 ms` — so `test_time_ms` is an **8-of-10-kernel aggregate**, and
`test_mem_bytes` the same 8. **`elixir` is the only language driven** (the tool's elixir env, with
`--no-docker` because no container runtime is available). **No rust row (`SysOnly`)**.

## 2. `emqx` — one rep, `skipped(emqx-requires-otp-28)` (unchanged, re-proved)

```
[skipped] emqx  sys   (emqx-requires-otp-28)
```
Verbatim reason string: `emqx-requires-otp-28`. Load 1.46 → 1.46; the run exits in 0 s (the gate
refuses before any build). The note's evidence, as printed this session:

* the **pin**: `EMQX pins Erlang/OTP 28.5.0.7-2 (env.sh:3 (export OTP_VSN=28.5.0.7-2))`;
* the **host**: ``the `erl` on PATH (…/erlang-29.1.1/lib/erlang/erts-17.1/bin/erl) is OTP 29
  (erts-17.1) — a combination EMQX does not build on``;
* **blocker (1)** the `:ezstd` NIF makefile's `env.mk` idiom (`erl -noshell -s init stop -eval …`)
  produces no output under OTP ≥ 29, so `mix compile` fails with `No rule to make target '…/env.mk'`;
* **blocker (2)** — re-probed on **this** run — `absent on this host right now: automake, libtool,
  cmake` (`:jq` needs automake+libtool+cmake; `:sasl_auth` needs `krb5.h`).

So it did **not** unexpectedly become measurable; no number exists to capture. The note also records
that blocker (2) is independent of the Erlang release (an OTP-28 store `erl` *does* evaluate the
idiom), i.e. swapping the oracle would only convert the honest skip into a build failure. Full ladder:
`results/m12-emqx-enablement-attempt.md`.

## 3. `--full` startup pass (both sides, `taskset -c 3`)

The milestone profile for startup: **50 runs** per side (so p99 is a meaningful statistic), plus the
5-run fixed compute loop and the separate VM-memory probe.

| metric | rust (`--full`) | sys (`--full`) | rust/sys |
|---|---|---|---|
| `startup_ms` p50 | **260.985 ms** (n=50, min 258.78, mean 261.26, p90 262.40) | **103.104 ms** (n=50, min 102.14, mean 103.22, p90 103.77) | **2.531×** |
| `startup_ms` p99 | **269.247 ms** (max 270.27) | **106.025 ms** (max 106.49) | **2.539×** |
| `compute_loop_ms` p50 | 279.639 ms (n=5, mean 279.51) | 164.362 ms (n=5, mean 164.38) | 1.701× |
| `compute_ops_per_sec` | 3 577 642 ops/s | 6 083 583 ops/s | 0.588× |
| `vm_memory_total_bytes` | 7 713 447 B | 36 196 168 B | 0.213× |
| `peak_memory_bytes` (VmHWM, OS RSS) | 153 853 952 B | 135 172 096 B | 1.138× |

Load rust 1.34 → 1.27, sys 1.27 → 1.22. **The two-sided row of record is the matched single-side pair
from stage 1** (six alternated rounds: p50 2.519×, p99 2.513×); this pass is the milestone-profile
`--full` confirmation, and it agrees with it to within 0.02×.
Read `compute_ops_per_sec` as the **native floor** (the loop is dominated by the natively-served
`lists:seq/2` + `lists:sum/1`), not as execution throughput, and `vm_memory_total_bytes` as an
**accounting figure**, not a footprint. Both sides produce all four metrics (`beam_rt` implements
`erlang:memory/0,1`), so nothing was withheld.

## 4. The latency-budget verdict block — complete, with `cowboy` evaluated

One `--runtime all --quick` pass over exactly the workloads the budget table needs:

```sh
erts_rust/target/release/bench run --quick --runtime all \
  --workloads baseline,list_walk,copy_bound,gc_bound,file_io,gc_class,cowboy \
  --out-file erts_rust/tools/bench/out/m19-budgetprobe-all-cowboy.json
```

Result: **`latency budgets — 11 met / 0 exceeded / 0 not measured`** — **all 11 rows evaluated**, and
`cowboy[sys] latency_ms p99 = 0.16 (limit <= 5)` is now **met** instead of `not-measured`
(stage 1's probe omitted `cowboy`, which left that row silent). Verbatim block:
`results/m19-budgetprobe-all-cowboy.txt`.

The evaluated figures in that block (this pass's own numbers — it is a **budget-evaluation** pass, not
a timing source: it is runtime-major, i.e. all `sys` rows then all `rust` rows, so it samples the two
sides minutes apart and carries the documented ordering bias):

* `baseline[rust] startup_ms` p50 283.38 / p99 293.31 (limits 800 / 2500), ratios 2.25× / 2.11× (limit 6×);
* `baseline[sys] startup_ms` p99 139.31 (limit 400);
* `cowboy[sys] latency_ms` **p99 0.157 ms** (limit 5) — `build_ms` 41 928, 248 727 requests,
  0 errors, 124 288 req/s, p50 0.061 ms, max 1.02 ms;
* `list_walk[rust]` 18.50 ns/element (limit 2 000); `copy_bound[rust]` `send_recv` 6 209 ns/op
  (limit 20 000); `gc_bound[rust]` `churn` 60 567 ns/op (limit 200 000);
* `file_io[rust]` `write_read` 97 677 ns/op (limit 200 000); `gc_class[rust]` `send_recv`
  50 749 ns/op (limit 200 000).

`cowboy[rust]` is `skipped(runtime-unsupported)` in this pass (nothing in the report, nothing in the
budget table depends on it). **No `Exceeded` anywhere.**

The pass was run **twice**: the first pass (16:17–16:21, load 1.13 → 0.95, 232 s) used the pre-touch-up
rationale text; after the `src/budget.rs` rationale rewrite it was re-run (16:24–16:28, load 3.73 →
0.71, 231 s) so that the archived report, its verbatim verdict block and the source agree. **The
archived `m19-budgetprobe-all-cowboy.{json,txt}` are the re-run**; the first pass's verdict block is
quoted only in that earlier form in the session log. Both passes read 11 met / 0 exceeded / 0 not
measured, and the budget table's limits were **not changed** — no measured figure breaches one.

## 5. Reproduction

```sh
# 0. build the driver + runtime once (from the repo root)
erts_rust/scripts/cargo.sh build --release -p beam_rt -p bench --manifest-path erts_rust/Cargo.toml --offline

# 1. three reps of each external target, one at a time, load logged before/after every pass
for id in erlc_compiler elixir_compiler cowboy khepri plb; do
  for n in 1 2 3; do
    erts_rust/target/release/bench run --quick --runtime sys --workloads "$id" \
      --out-file erts_rust/tools/bench/out/m19-ext-$id-r$n.json
  done
done
# 2. emqx, once (expected: skipped(emqx-requires-otp-28))
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx \
  --out-file erts_rust/tools/bench/out/m19-ext-emqx.json
# 3. the milestone-profile startup pass, both sides, pinned
taskset -c 3 erts_rust/target/release/bench run --full --runtime rust --workloads baseline \
  --out-file erts_rust/tools/bench/out/m19-full-baseline-rust.json
taskset -c 3 erts_rust/target/release/bench run --full --runtime sys  --workloads baseline \
  --out-file erts_rust/tools/bench/out/m19-full-baseline-sys.json
# 4. the budget-evaluation pass (both sides in ONE report so the ratio rows resolve)
erts_rust/target/release/bench run --quick --runtime all \
  --workloads baseline,list_walk,copy_bound,gc_bound,file_io,gc_class,cowboy \
  --out-file erts_rust/tools/bench/out/m19-budgetprobe-all-cowboy.json
```
The verdict block is the `latency budgets — …` section on stderr; archive it with
`sed -n '/^latency budgets/,$p' <log>`. `--fail-on-budget` makes an `Exceeded` an exit `1`
(there was none to make it fire).

## 6. Files

| file | what |
|---|---|
| `results/m19-ext-{erlc_compiler,elixir_compiler,cowboy,khepri,plb}-r{1,2,3}.json` | the 15 external reps |
| `results/m19-ext-emqx.json` | the single honest skip |
| `results/m19-full-baseline-{rust,sys}.json` | the `--full` startup pass, one per side |
| `results/m19-budgetprobe-all-cowboy.json` / `.txt` | the budget-evaluation pass + its verbatim verdict block |
| `results/m19-ext-load.log` | `/proc/loadavg` before/after every pass (40 readings, 1-min 0.71–3.73) |
