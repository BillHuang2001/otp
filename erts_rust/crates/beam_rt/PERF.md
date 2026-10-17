# `beam_rt` — current performance figures (`CURRENT`)

The present-tree measurement record for the Rust runtime: what `beam_rt` costs today on both sides of the comparison, with every figure's qualification attached.
Every number in this file is **measured, never modelled**: nothing here is fabricated, and a sys-only workload that cannot produce a Rust-side figure is recorded as `skipped(<reason>)` with no number.
It is **not** a change log — [`../../../CHANGE.md`](../../../CHANGE.md) records what changed, this file records what it cost.
This file owns the **`CURRENT` snapshot** and the standing *Current startup regime* note that qualifies every `startup_ms` figure in the crate.
The rest of the record lives in two siblings: [`PERF_HISTORY.md`](PERF_HISTORY.md) holds the dated **`BEFORE` → `AFTER`** snapshots (the historical baselines and the delta between them), and [`PERF_DISPATCH.md`](PERF_DISPATCH.md) holds the per-call **`DISPATCH-CUT`** / **`COPY-OUT-CUT`** analyses.
Route by question: today's cross-runtime figures and the execution-tier effect → this file; a dated baseline or a `BEFORE`→`AFTER` delta → `PERF_HISTORY.md`; registered-BIF per-call cost, the dispatch prologue or the BIF-result copy-out → `PERF_DISPATCH.md`.
The measurement narrative lives in [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md); the bench driver's own environment record, per-target tables and methodology are [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md), which this file links to and does not reproduce.

## Current startup regime (read before quoting any `startup_ms` figure)

Every CLI action except `-version` now drives a boot script before the action's own start instruction: the explicit `-boot FILE` value when given, else **this runtime's own** release's committed `bootstrap/bin/start.boot` (`CodeServer::own_bootstrap_boot_script`; the literal `start` resolves to `<Root>/bin/start.boot` only when the repo bootstrap is undiscoverable, e.g. an installed deployment).
A bare `-eval 'ok.'` invocation therefore boots `start.boot` and is measured at **≈ 0.55 s wall** on this host (555/557/563/643 ms over 5 runs at 1-min loadavg ≈ 1.1, so absolute ms are host-dependent).
Every `startup_ms` figure in this file and in [`PERF_HISTORY.md`](PERF_HISTORY.md) is **that snapshot's own** preloaded-only-slice measurement (the ≈ 14–18 ms band) and its ratio to the oracle is that snapshot's; those numbers do not describe the current whole-process wall.
The present whole-process wall is **CPU-bound and carries no fixed waits**: `strace -c -f` shows **0 `clock_nanosleep`** calls and only **zero-timeout** `epoll_wait` calls (12 calls, ≈ 6 µs total), so a blocked round returns instead of sleeping and the residual ≈ 0.55 s is engine work (the oracle's own `erl -noshell -eval 'halt(0).'` boot is ≈ 0.12–0.15 s here).
The wait a *blocked* pass pays is mechanism-agnostic, so read it as a property — bounded, never a spin — not as one syscall name: with a live port in the table (a booted kernel keeps `inet_gethost` open, which makes `PortRegistry::has_nothing_to_poll()` false) the pass's wait is the port poll's blocking `epoll_wait(PORT_POLL_SLICE)`, not the timer sleep, and only a round that ran **zero** slices is allowed to pay it.
Because that wall is now this runtime's own CPU cost rather than a fixed sleep, **no startup ratio measured against the oracle was re-recorded at this HEAD** — the committed `startup` ratios belong to the dated snapshots in [`PERF_HISTORY.md`](PERF_HISTORY.md) — so the `startup_ms` metric should be **re-recorded at the next benchmark milestone** before any current startup advantage or loss is quoted.
The `foreign`/`list_walk`/`copy_bound`/`gc_bound` **startup-cancelled marginals** still cancel whatever startup a sample pays (`(mean wall@N − mean wall@0) / N`), but every Rust sample now carries the ≈ 0.55 s boot, so those rows measure the loop against a far larger whole-process wall than the dated snapshots in [`PERF_HISTORY.md`](PERF_HISTORY.md) record.

## `CURRENT` snapshot — HEAD `da82ffe33c`

### Method

| item | value |
|---|---|
| HEAD | `da82ffe33c0896eeb99385ed32ea3eb17a7ed814` (`da82ffe33c`, 2026-10-12 06:18 +0800) — the merge of the two commits that landed the runner's per-instruction reduction charge and `RuntimeContext`'s `reduce` opt-out |
| Date (UTC) | 2026-10-11 22:14–22:40 UTC |
| Host / kernel | Linux 7.2.9 `x86_64`, `nproc` = **14** — the **same shared, oversubscribed** host, but this pass ran in a **light window** (see the caveat) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl`, **OTP 29.1.1 / erts-17.1** (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/.../bin/erl`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| N | **3** default (`JitMode::Auto`) passes **+ 3** `BEAM_RT_TIER=interpreter` passes; each set reported as the **min over its 3**, with the observed range quoted |
| Host load (1-min `/proc/loadavg`) | default passes `13.64 → 8.12`, `7.47 → 1.99`, `1.99 → 1.27`; interpreter passes `0.77 → 1.45`, `1.45 → 1.13`, `1.04 → 1.35` — i.e. **≈ 0.8–14** (mostly < 8), far below `BEFORE`/`AFTER`'s ≈ 20–55 |

#### Commands

```sh
# build the runtime and the driver
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p bench -p beam_rt

# 3 x the focused cross-runtime pass on the CURRENT default (Auto => baseline JIT)
for i in 1 2 3; do
  erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file $TMPDIR/perf-now/full$i.json
  erts_rust/target/release/bench compare $TMPDIR/perf-now/full$i.json   # rust/sys within one report
done

# 3 x the SAME pass with the interpreter pinned — the tier-attribution point
for i in 1 2 3; do
  BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file $TMPDIR/perf-now/interp$i.json
done
```

`bench` spawns the runtime with an **inherited environment**, so exporting `BEAM_RT_TIER=interpreter`
on the driver reaches every spawned `beam_rt` (the same mechanism `AFTER` uses). The raw harness JSON
was written under `$TMPDIR/perf-now/` and is **ephemeral** (not committed — re-run the commands above to regenerate).

#### Host-load caveat (read before quoting any absolute number)

The host is the same shared/oversubscribed one `BEFORE`/`AFTER` describe, but this pass ran in a
**light window**: the 1-min `/proc/loadavg` stayed at **≈ 0.8–14** (mostly below 8) against
`BEFORE`/`AFTER`'s ≈ 20–55. **Absolute latencies here are therefore lower than the two earlier
snapshots for that reason alone** — `sys` startup `min` reads **98.64 ms** against `AFTER`'s 160.47 ms,
although the `sys` runtime did not change between the snapshots. **Absolute latencies are not
comparable across the three snapshots**; only the **ratios** and the **minima** are signal, and the
driver is runtime-major (all `sys` rows, then all `rust` rows) within one report, so the mitigation is
again the three **block-interleaved** passes (`sys,rust | sys,rust | sys,rust`). Read the `CURRENT`
figures as a **loaded-host measurement in a light window**, not a clean-machine number.

### Current figures (`rust` / `sys`, min of 3, `--full` focused cross-runtime pass)

Same reading convention as `BEFORE`/`AFTER` in [`PERF_HISTORY.md`](PERF_HISTORY.md) (**< 1 ⇒ Rust cheaper, > 1 ⇒ Rust dearer**; the ratio is
the headline). `sys` is the min-of-3 over the default set (the oracle does not see the tier); the two
`rust` columns are the min-of-3 of the **default** and of the **`BEAM_RT_TIER=interpreter`** sets.
`slice` is per-case **whole-process** wall (one call per process); the
`foreign`/`list_walk`/`copy_bound`/`gc_bound` rows are **startup-cancelled marginals**
(`(mean wall@N − mean wall@0) / N`), the interpreter-throughput signal.
This snapshot's `baseline`/`slice` figures are its own preloaded-only-slice measurements; the current runtime pays the boot script on every action (see *Current startup regime*), so they are not the current whole-process wall.

| workload | case | metric (unit) | **rust/sys default (min3)** | range | rust/sys INTERP (min3) | sys (min3) | rust default (min3) | rust INTERP (min3) |
|---|---|---|---|---|---|---|---|---|
| `baseline` | — | `startup_ms` (ms) | **0.141** | 0.141–0.168 | 0.144 | 98.64 | 14.56 | 13.99 |
| `slice` | `add` | `wall_ms` (ms) | **0.104** | 0.104–0.139 | 0.117 | 110.61 | 15.36 | 13.88 |
| `slice` | `fib` | `wall_ms` (ms) | **0.107** | 0.107–0.141 | 0.134 | 110.34 | 12.77 | 15.38 |
| `slice` | `pick` | `wall_ms` (ms) | **0.120** | 0.120–0.132 | 0.120 | 107.55 | 14.11 | 14.13 |
| `slice` | `calendar_last_day` | `wall_ms` (ms) | **0.124** | 0.124–0.154 | 0.120 | 112.71 | 15.47 | 14.76 |
| `slice` | `queue_from_size` | `wall_ms` (ms) | **0.185** | 0.185–0.197 | 0.137 | 107.66 | 20.80 | 17.09 |
| `slice` | `gb_trees_size` | `wall_ms` (ms) | **0.124** | 0.124–0.151 | 0.126 | 113.64 | 14.62 | 14.71 |
| `slice` | `base64_encode_size` | `wall_ms` (ms) | **0.118** | 0.118–0.145 | 0.138 | 109.70 | 14.42 | 16.23 |
| `slice` | `ordsets_from_size` | `wall_ms` (ms) | **0.124** | 0.124–0.133 | 0.130 | 117.64 | 14.59 | 15.68 |
| `slice` | `string_str_length` | `wall_ms` (ms) | **0.297** | 0.297–0.333 | 0.238 | 113.40 | 35.34 | 27.28 |
| `foreign` | `cowboy_bstr:to_lower/1` | `ns_per_call` (ns) | **46.75** | 46.755–48.989 | 55.57 | 343.00 | 16 453.22 | 18 832.03 |
| `foreign` | `cowboy_bstr:capitalize_token/1` | `ns_per_call` (ns) | **64.46** | 64.463–68.124 | 76.29 | 304.17 | 20 374.27 | 23 692.45 |
| `foreign` | `emqx_base62:encode/1` | `ns_per_call` (ns) | **41.48** | 41.484–44.988 | 59.43 | 190.51 | 8 542.00 | 11 672.37 |
| `foreign` | `emqx_base62:decode/1` | `ns_per_call` (ns) | **49.74** | 49.736–52.828 | 70.23 | 289.88 | 14 996.48 | 20 045.53 |
| `list_walk` | — | `ns_per_element` (ns) | **58.14** | 58.145–60.106 | 63.74 | 2.45 | 143.40 | 156.15 |
| `copy_bound` | `etf_roundtrip` | `ns_per_op` (ns) | **109.05** | 109.046–113.693 | 111.01 | 650.76 | 71 597.74 | 72 285.68 |
| `copy_bound` | `ets_copy` | `ns_per_op` (ns) | **13.95** | 13.952–14.181 | 14.28 | 222.17 | 3 150.75 | 3 216.32 |
| `copy_bound` | `send_recv` | `ns_per_op` (ns) | **145.51** | 145.511–150.696 | 101.65 | 1 455.51 | 215 321.19 | 147 862.60 |
| `gc_bound` | `churn` | `ns_per_op` (ns) | **29.90** | 29.904–30.194 | 55.18 | 4 283.39 | 129 331.85 | 240 007.25 |

**Reading.** Whole-process cost at this HEAD was still far lower under Rust (`baseline` startup
**0.141×**, `slice` cases **0.104×–0.297×** — this snapshot's preloaded-only slice, not the boot-paying
regime of *Current startup regime*), while the per-unit interpreter cost is still far higher
(**13.9×–145.5×** across the startup-cancelled marginals). `slice` peak RSS remains the one
like-for-like cross-runtime memory row (one call per process): `sys` **71.57 MB**, `rust` **27.48 MB**
→ rust **2.60× lower** (essentially unchanged from `AFTER`'s 2.46×); the loop workloads' rust peak RSS
grows with the per-runtime loop count `N` (`RESULTS.md` §6.7) and is **not** comparable. The `foreign`
per-call gap vs the system runtime (**41×–64×**) sits in the same band as `AFTER`'s (**37×–55×**) — the
`CURRENT` default runs the baseline tier on the *whole* process, so this is the tier-inclusive figure.

### `rust` default vs `BEAM_RT_TIER=interpreter` — the tier effect (min of 3)

Both columns are the **min of 3** of the `rust` metric; `< 1×` means the baseline JIT is faster.

| workload | case | metric | default (min3) | `BEAM_RT_TIER=interpreter` (min3) | default/interp |
|---|---|---|---|---|---|
| `baseline` | — | `startup_ms` | 14.56 | 13.99 | 1.041× |
| `slice` | `add` | `wall_ms` | 15.36 | 13.88 | 1.106× |
| `slice` | `fib` | `wall_ms` | 12.77 | 15.38 | 0.831× |
| `slice` | `pick` | `wall_ms` | 14.11 | 14.13 | 0.998× |
| `slice` | `calendar_last_day` | `wall_ms` | 15.47 | 14.76 | 1.048× |
| `slice` | `queue_from_size` | `wall_ms` | 20.80 | 17.09 | **1.217× worse** |
| `slice` | `gb_trees_size` | `wall_ms` | 14.62 | 14.71 | 0.994× |
| `slice` | `base64_encode_size` | `wall_ms` | 14.42 | 16.23 | 0.889× |
| `slice` | `ordsets_from_size` | `wall_ms` | 14.59 | 15.68 | 0.931× |
| `slice` | `string_str_length` | `wall_ms` | 35.34 | 27.28 | **1.295× worse** |
| `foreign` | `cowboy_bstr:to_lower/1` | `ns_per_call` | 16 453.22 | 18 832.03 | **0.874×** |
| `foreign` | `cowboy_bstr:capitalize_token/1` | `ns_per_call` | 20 374.27 | 23 692.45 | **0.860×** |
| `foreign` | `emqx_base62:encode/1` | `ns_per_call` | 8 542.00 | 11 672.37 | **0.732×** |
| `foreign` | `emqx_base62:decode/1` | `ns_per_call` | 14 996.48 | 20 045.53 | **0.748×** |
| `list_walk` | — | `ns_per_element` | 143.40 | 156.15 | **0.918×** |
| `copy_bound` | `etf_roundtrip` | `ns_per_op` | 71 597.74 | 72 285.68 | 0.990× |
| `copy_bound` | `ets_copy` | `ns_per_op` | 3 150.75 | 3 216.32 | 0.980× |
| `copy_bound` | `send_recv` | `ns_per_op` | 215 321.19 | 147 862.60 | **1.456× worse** |
| `gc_bound` | `churn` | `ns_per_op` | 129 331.85 | 240 007.25 | **0.539×** |

**Reading the tier effect.** The baseline JIT is a clear win for **`foreign`** (**0.73×–0.87×**) and
**`gc_bound churn`** (**0.539×**), and neutral for `baseline`, `list_walk` and most `slice` cases —
where the whole-process wall is dominated by `beam_rt` startup (this snapshot's preloaded-only slice; the
current runtime pays the ≈ 0.55 s boot-script drive on every action, see *Current startup regime*), not
the interpreted loop. The only real
regressions are the **module-graph-heavy one-shot** rows, `slice`'s `string_str_length` (**1.295×**),
`queue_from_size` (**1.217×**) and `copy_bound`'s `send_recv` (**1.456×**): the first-entry compile cost
the section below characterises. `fib`/`base64_encode_size`/`ordsets_from_size` read *better* on the
default column, but by less than the per-pass spread (the `slice` range column above is up to ≈35 %), so
they are not attributed to the tier.

### The one-shot cold-start cost — current characterisation

The tier's cost is a **one-shot, first-entry compile** that a short-lived process cannot amortise. The
engine's baseline driver now compiles **lazily per function** (on first entry of a `(func, pc)`, one
sealed mapping per function; the eager whole-module `JitBackend::compile` is retained only for the
whole-module introspection surface and `jit_bench`), so the regression a module-graph-heavy one-shot
process pays is now **small**. Measured here at this HEAD with the same `--full` focused pass, **3
default + 3 `BEAM_RT_TIER=interpreter` passes**, each column the **min over its 3** (host 1-min
`/proc/loadavg` ≈ 0.8–14 throughout):

| workload | case | metric | default (min3) | `BEAM_RT_TIER=interpreter` (min3) | default/interp |
|---|---|---|---|---|---|
| `slice` | `string_str_length` | `wall_ms` | **35.34** | **27.28** | **1.295× worse** |
| `slice` | `queue_from_size` | `wall_ms` | 20.80 | 17.09 | 1.217× worse |
| `slice` | `add` / `fib` | `wall_ms` | 15.36 / 12.77 | 13.88 / 15.38 | 1.106× / 0.831× |
| `foreign` | `cowboy_bstr:to_lower/1` | `ns_per_call` | 16 453 | 18 832 | **0.874×** |
| `foreign` | `cowboy_bstr:capitalize_token/1` | `ns_per_call` | 20 374 | 23 692 | **0.860×** |
| `foreign` | `emqx_base62:encode/1` | `ns_per_call` | 8 542 | 11 672 | **0.732×** |
| `foreign` | `emqx_base62:decode/1` | `ns_per_call` | 14 996 | 20 046 | **0.748×** |
| `list_walk` | — | `ns_per_element` | 143.40 | 156.15 | **0.918×** |
| `copy_bound` | `etf_roundtrip` / `ets_copy` | `ns_per_op` | 71 598 / 3 151 | 72 286 / 3 216 | 0.990× / 0.980× |
| `copy_bound` | `send_recv` | `ns_per_op` | 215 321 | 147 863 | **1.456× worse** |
| `gc_bound` | `churn` | `ns_per_op` | 129 332 | 240 007 | **0.539×** |
| `baseline` | — | `startup_ms` | 14.56 | 13.99 | 1.041× |

So `string_str_length`'s whole-process regression over the interpreter is **≈ 1.3×** here (min-of-3),
down from the `AFTER`-HEAD's **11.27×** under the eager whole-module compile (the eager figure for the
same shape measured **≈ 12.3×** at that HEAD). `crates/beam_engine/MEASURED.md` records the same
behaviour independently for the `string_str_length` shape — the `beam_rt run <bslice_string.beam>
bslice_string:str_length/1 '"hello"'` repro, **N=30 interleaved samples**: default (baseline-JIT) **min
31.86 ms** vs `BEAM_RT_TIER=interpreter` **min 25.51 ms** → **≈ 1.22–1.25×**, and the equivalent
`tools/bench` `slice` workload read **31.97 ms default / 31.92 ms interpreter** (min-of-3) — against the
eager whole-module compile it replaced at **405.29 ms / 33.68 ms → ≈ 12.3×**. The point estimate depends
on host load and on how many of the *entered* functions the process actually runs, so read **≈ 1.2–1.3×**
for this shape, not one number.

**`jit_bench` caveat.** `beam_engine`'s `jit_bench` drives the **eager** whole-module artifact only
(`JitBackend::compile`), so it pins the eager/hot path and does **not** capture the lazy per-entry path.
The lazy path is measured by this focused cross-runtime pass and by `crates/beam_engine/MEASURED.md`'s
cold-start section, never by `jit_bench`.

The shape of what remains (the `AFTER` analysis minus the now-removed eager whole-module compile):

- It is a **per-`beam_rt`-OS-process** cold start, not a per-call or a per-process cost: the
  compiled-function cache is one `thread_local!` per OS thread, shared by every process of one runtime,
  never across OS processes, so a long-running node compiles a given function **once** and every later
  process on it reuses the artifact.
- `tools/bench/src/workloads/slice.rs`'s `execute_case` spawns a **new OS process per sample**
  (`runs_for(Full) = 25`) and `slice` is scored as **whole-process wall**, so every sample pays the
  first-entry compile of exactly the functions it enters — which is why the module-graph-heavy `slice`
  rows are the ones that move.
- On the **native** tier a natively linked loop still charges no reduction mid-block (it charges only at
  a deopt or a heap-admission boundary — see the crate `CONTEXT.md`); on the **interpreter** tier the
  budget is now charged per interpreted instruction, so a non-allocating loop *is* pre-emptible there.
  That is a pre-emption property, not a cost one: the two columns above differ only by the tier.

#### The interpreter path's per-instruction reduction charge — controlled A/B

The `INTERP` column above and `### The one-shot cold-start cost` both compare **tiers**; neither
isolates the cost of the **per-instruction reduction charge** itself. Under
`BEAM_RT_TIER=interpreter` every scheduled process pays, for each interpreted instruction, one
`&mut dyn ExecutionContext` crossing plus a u32 decrement/compare: `Machine::drive` calls
`ctx.reduce(1)?` whenever it latched `wants_reduce() == true`
(`crates/beam_engine/src/machine/drive.rs`), and `runner/budget.rs::BudgetContext` is the only host
that answers `true` (the single-pass `RuntimeContext` answers `false`, so those runs skip the
crossing). A controlled A/B at **one HEAD**, with nothing else changed, measures what that charge
costs.

| variant | temporary edit (never committed) | what it removes |
|---|---|---|
| **A — charge ON** | none — the shipped behaviour | — |
| **B — charge OFF** | `BudgetContext::wants_reduce() -> false` | the **hook crossing**, the charge, **and** the slice pre-emptions the charge induced on any stretch that reaches no heap-admission point |

**Variant (ii) was chosen deliberately**, over the alternative (keep `wants_reduce() == true`, make
`reduce` return `Ok(())`): that variant would have measured the charge *arithmetic* alone, whereas
variant (ii) removes the whole mechanism and is therefore the **full-cost upper bound** on the
per-instruction dyn call. Read every delta below as that upper bound, not as pure call overhead.

| item | value |
|---|---|
| HEAD (one, for both variants) | `06bf44d962306b7ae91891e040f734919a494b1f` (`06bf44d962`, 2026-10-12 06:49 +0800) |
| Date (UTC) | 2026-10-11 23:10–23:17 UTC |
| Host / kernel | Linux 7.2.9 `x86_64`, `nproc` = **14** (the same shared, oversubscribed host) |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Tier | `BEAM_RT_TIER=interpreter` (the charge is unreachable mid-block on the default baseline tier) |
| Runtime side | `--runtime rust` only — the system-ERTS side cannot see this charge and would only double the wall time |
| N | **5 per variant**, **alternated at run granularity** (`A B A B …`, 10 runs in ≈ 7 min) |
| Host load (1-min `/proc/loadavg` immediately **before** each run) | A `1.06, 1.17, 1.01, 1.00, 1.00`; B `1.18, 1.18, 1.07, 1.00, 0.93` — ≈ **0.9–1.2** on a 14-CPU host, and ≤ 1.25 after every run (a light window: the `BEFORE`/`AFTER` load caveat does **not** apply to this pass) |
| per-run figure | the report's `summary.min` for a sampled metric (`startup_ms`, `case_*_wall_ms`; 10 samples/case/run, 50 for `baseline` startup) and the scalar `value` for a throughput metric (`ns_per_call`/`ns_per_op`/`ns_per_element`) |
| reported figure | **min over the 5** runs of that variant; the spread is that variant's full min–max over its 5 runs |

```sh
# 1. build the runtime + driver in release; keep the shipped binary aside as A
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p bench -p beam_rt
cp erts_rust/target/release/beam_rt $TMPDIR/ab/beam_rt.charge            # A — charge ON

# 2. TEMPORARY, NON-COMMITTED edit to src/runner/budget.rs:
#      fn wants_reduce(&self) -> bool { false }
#    rebuild and keep the binary aside as B
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p beam_rt
cp erts_rust/target/release/beam_rt $TMPDIR/ab/beam_rt.nocharge         # B — charge OFF

# 3. revert the edit completely — `git diff src/runner/budget.rs` is empty in the measured tree

# 4. ten interleaved runs, host load recorded before each
for v in charge nocharge charge nocharge charge nocharge charge nocharge charge nocharge; do
  cp $TMPDIR/ab/beam_rt.$v erts_rust/target/release/beam_rt
  cat /proc/loadavg >> $TMPDIR/ab/loadavg.log
  BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --full --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound --out-file $TMPDIR/ab/runN.json
done
```

All ten reports are `status=ok` with **no** unsupported case and **no** below-timer-resolution note,
so every row below is a real measurement (the raw JSON was written under `$TMPDIR/ab/` and is
**ephemeral**, not committed — re-run the loop to regenerate).

Result — `A/B > 1` ⇒ charge ON is dearer; the variant removed `Δ = A − B`. `baseline`/`slice` rows are
**ms** (`startup_ms` / `case_*_wall_ms`); every other row is **ns** per unit.

| workload | case | A (min5) | A range (5) | B (min5) | B range (5) | Δ (A−B) | A/B | verdict |
|---|---|---|---|---|---|---|---|---|
| `baseline` | — | 13.778 | 13.778–29.519 | 13.733 | 13.733–22.782 | +0.045 | 1.003× | below the spread |
| `slice` | `add` | 13.944 | 13.944–22.224 | 14.041 | 14.041–18.066 | −0.097 | 0.993× | below the spread |
| `slice` | `fib` | 14.076 | 14.076–22.308 | 14.043 | 14.043–25.821 | +0.033 | 1.002× | below the spread |
| `slice` | `pick` | 13.994 | 13.994–15.611 | 14.080 | 14.080–22.873 | −0.086 | 0.994× | below the spread |
| `slice` | `calendar_last_day` | 14.654 | 14.654–25.223 | 14.842 | 14.842–25.910 | −0.187 | 0.987× | below the spread |
| `slice` | `queue_from_size` | 17.081 | 17.081–25.941 | 18.193 | 18.193–23.692 | −1.112 | 0.939× | below the spread |
| `slice` | `gb_trees_size` | 14.779 | 14.779–25.063 | 14.735 | 14.735–22.510 | +0.044 | 1.003× | below the spread |
| `slice` | `base64_encode_size` | 15.314 | 15.314–26.073 | 16.065 | 16.065–25.861 | −0.752 | 0.953× | below the spread |
| `slice` | `ordsets_from_size` | 14.401 | 14.401–22.709 | 15.486 | 15.486–22.813 | −1.085 | 0.930× | below the spread |
| `slice` | `string_str_length` | 27.424 | 27.424–33.580 | 29.850 | 29.850–33.510 | −2.426 | 0.919× | below the spread |
| `foreign` | `cowboy_bstr:to_lower/1` | 18 911 | 18 911–19 594 | 18 353 | 18 353–18 798 | +558 | 1.030× | disjoint ranges; Δ inside A's own range |
| `foreign` | `cowboy_bstr:capitalize_token/1` | 23 739 | 23 739–23 884 | 22 857 | 22 857–23 297 | +882 | 1.039× | **above the spread** (ranges disjoint) |
| `foreign` | `emqx_base62:encode/1` | 11 625 | 11 625–11 996 | 11 259 | 11 259–11 533 | +366 | 1.032× | disjoint ranges; Δ inside A's own range |
| `foreign` | `emqx_base62:decode/1` | 19 998 | 19 998–20 499 | 17 403 | 17 403–17 861 | +2 595 | 1.149× | **above the spread** (ranges disjoint) |
| `copy_bound` | `etf_roundtrip` | 72 172 | 72 172–73 961 | 72 560 | 72 560–73 646 | −389 | 0.995× | below the spread |
| `copy_bound` | `ets_copy` | 3 192 | 3 192–3 261 | 3 084 | 3 084–3 205 | +108 | 1.035× | below the spread |
| `copy_bound` | `send_recv` | 146 599 | 146 599–150 600 | 147 884 | 147 884–150 961 | −1 285 | 0.991× | below the spread |
| `list_walk` | — | 145.6 | 145.6–163.0 | 134.3 | 134.3–145.5 | +11.3 | 1.084× | disjoint by 0.1 ns; Δ inside A's own range |
| `gc_bound` | `churn` | 239 053 | 239 053–245 901 | 237 677 | 237 677–240 416 | +1 376 | 1.006× | below the spread |

**Verdict.**

- **Above the spread — the charge's full cost is resolved:** `emqx_base62:decode/1` **+14.9 %**
  (19 998 → 17 403 ns) and `cowboy_bstr:capitalize_token/1` **+3.9 %** (23 739 → 22 857 ns), both with
  disjoint 5-run ranges and a Δ exceeding the larger range.
- **Sign consistent, magnitude at the noise floor:** `cowboy_bstr:to_lower/1` (+3.0 %),
  `emqx_base62:encode/1` (+3.3 %) and `list_walk` (+8.4 %) have **disjoint** 5-run ranges (5/5
  rank-separated) but a delta of minima *inside* the larger set's own min–max range, so the sign is
  rank-separable and the **magnitude is not separable at this N**.
- **Not separable from the noise floor, sign not even consistent:** every `baseline` and `slice` row, all
  three `copy_bound` rows and `gc_bound churn` (overlapping ranges; several read *cheaper* under A).

Which rows could show it at all:

- **Process-startup-dominated — cannot show it.** `baseline` `startup_ms` (≈ 13.7 ms of process boot —
  this snapshot's preloaded-only slice; the current runtime instead pays the ≈ 0.55 s boot-script drive,
  see *Current startup regime*) and every `slice` row (whole-process wall for one call per OS process,
  13.9–27.4 ms), of which the interpreted work is a small fraction.
- **Interpreted-work marginals — the rows that *can* show it.** `foreign` (per `cowboy_bstr` /
  `emqx_base62` call) is the only family that resolved it, at **+3.0 % to +14.9 %** of per-call time.
  `copy_bound`'s three cases are dominated by the copy itself and `gc_bound churn` by the
  allocate-and-abandon work plus any collection (≈ 239 µs/op), so one reduction charge is a small share
  there — and their startup-cancelled marginals are noisier (2–4 % min–max per set), leaving it
  unresolved at N = 5.
- The case-to-case deltas span **+3.0 % to +14.9 %** while the disjoint-but-inside-spread rows sit at
  ≈ +3 % to +8 %, so the charge is **not** a single per-instruction constant across these workloads —
  consistent with variant (ii) also removing the **pre-emptions** the charge induced, which is why the
  figures are an **upper bound** on the pure dyn call.

**Take-away for the tree.** On the **interpreter** tier the shipped per-instruction charge costs up to
**≈ 15 %** of per-call time on the most instruction-bound workload measured here and **≈ 3–4 %** on
the other three `foreign` shapes; on the **default baseline-JIT** tier the hook is unreachable
mid-block, so this cost is specific to the interpreter path and no startup-dominated or
copy/GC-dominated row resolves it at N = 5. Nothing here changes the pre-emption property the charge
exists for (see `src/runner/budget.rs`) — it only prices it.

### Caveats that invalidate a comparison (`CURRENT`)

- **Absolute latencies are load-polluted**, and this pass ran in a **light window** (≈ 0.8–14 versus
  `BEFORE`/`AFTER`'s ≈ 20–55); only ratios/minima are signal. Do **not** compare absolute ms/ns across
  the three snapshots (`sys` startup `min` 98.64 ms here vs 160.47 ms `AFTER` is host load alone).
- **The `CURRENT` figures are min-of-3 within their own mode**, exactly as `BEFORE`/`AFTER`'s are; the
  `sys` and `rust` columns of a ratio can therefore come from *different* passes.
- **Nothing was skipped**: all six `Runtimes::Both` workloads ran in all six passes (`status=ok`) on
  both runtimes. The `SysOnly` external targets (`cowboy`, `khepri`, `plb`, `emqx`, `erlc_compiler`,
  `elixir_compiler`) were **not run** in this pass — sys-only, so there is no Rust-side figure to
  snapshot (`BEFORE`/`AFTER` record `emqx` as `skipped(emqx-requires-otp-28)`).

### See Also

- [`PERF_HISTORY.md`](PERF_HISTORY.md) → the dated `BEFORE`/`AFTER` snapshots and the `BEFORE`→`AFTER` delta behind the tier-default and dispatch-prologue changes.
- [`PERF_DISPATCH.md`](PERF_DISPATCH.md) → the per-call `DISPATCH-CUT` / `COPY-OUT-CUT` analyses, i.e. the `examples/perf_call.rs` figures with their reproduction recipes.
- [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md) → the bench driver's authoritative environment record, per-target tables, methodology and honesty audit (this file links to it and does not reproduce it). Its `m6` bands are a **different, far more heavily loaded** pass and are not the `CURRENT` baseline.
- [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) → the `perf_bif` harness, the dispatch-cost narrative and the execution-trace seam.
- [`CONTEXT.md`](CONTEXT.md) → the runtime crate's contracts and current state.
