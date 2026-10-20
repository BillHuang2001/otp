# `beam_rt` — current performance figures (`CURRENT`)

The present-tree measurement record for the Rust runtime: what `beam_rt` costs today on both sides of the comparison, with every figure's qualification attached.
Every number in this file is **measured, never modelled**: nothing here is fabricated, and a sys-only workload that cannot produce a Rust-side figure is recorded as `skipped(<reason>)` with no number.
It is **not** a change log — [`../../../CHANGE.md`](../../../CHANGE.md) records what changed, this file records what it cost.
This file owns the **`CURRENT` snapshot**, the standing *Current startup regime* note that qualifies every `startup_ms` figure in the crate, and the boot path's poll-budget note (*The boot path's one live-but-idle port, and the poll budget*).
The rest of the record lives in two siblings: [`PERF_HISTORY.md`](PERF_HISTORY.md) holds the dated **`BEFORE` → `AFTER`** snapshots (the historical baselines, the delta between them, and the single-mechanism controlled A/Bs), and [`PERF_DISPATCH.md`](PERF_DISPATCH.md) holds the per-call **`DISPATCH-CUT`** / **`COPY-OUT-CUT`** analyses.
Route by question: today's cross-runtime figures and the execution-tier effect → this file; a dated baseline or a whole-suite `BEFORE`→`AFTER` snapshot → `PERF_HISTORY.md`; a **controlled A/B at one change**, carrying its own two HEADs → `PERF_HISTORY.md`; registered-BIF per-call cost, the dispatch prologue or the BIF-result copy-out → `PERF_DISPATCH.md`.
The measurement narrative lives in [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md); the bench driver's own environment record, per-target tables and methodology are [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md), which this file links to and does not reproduce.

## Current startup regime (read before quoting any `startup_ms` figure)

Every CLI action except `-version` now drives a boot script before the action's own start instruction: the explicit `-boot FILE` value when given, else **this runtime's own** release's committed `bootstrap/bin/start.boot` (`CodeServer::own_bootstrap_boot_script`; the literal `start` resolves to `<Root>/bin/start.boot` only when the repo bootstrap is undiscoverable, e.g. an installed deployment).
A bare `-eval 'ok.'` invocation therefore boots `start.boot` and is measured at **≈ 0.35 s wall** on this host: paired medians of 12 alternating `taskset -c 3` rounds read **340.5 ms wall over 328.5 ms CPU** for `beam_rt -noshell -eval 'halt().'` (1-min `/proc/loadavg` ≈ 20 after the last round), i.e. CPU work plus whatever the shared host adds — *The boot path's one live-but-idle port, and the poll budget* below carries that measurement in full.
Every other `startup_ms` figure in this file and in [`PERF_HISTORY.md`](PERF_HISTORY.md) is **that snapshot's own** preloaded-only-slice measurement (the ≈ 14–18 ms band), so it does not describe the current whole-process wall.
The present whole-process wall is **CPU-bound and carries no fixed waits**: `strace -f -c -w` over a whole run shows **0 `clock_nanosleep`** calls and **≈ 4 ms** of `epoll_wait` in total, so a blocked round returns instead of sleeping and the residual wall is engine work (the oracle's own `erl -noshell -eval 'halt(0).'` boot is ≈ 0.12–0.15 s here).
The wait a *blocked* pass pays is therefore mechanism-agnostic — bounded, never a spin, not one syscall name: with a port live in the table (a booted kernel keeps the `inet_gethost` helper child open, which makes `PortRegistry::has_nothing_to_poll()` false from directive 18 on) the wait is that port's poll, its blocking form is taken only while a port **owes a reply**, and only by a round that ran **zero** slices (*The boot path's one live-but-idle port, and the poll budget*).
The cross-runtime `startup_ms` ratios that do describe the present tree are **2.649** against the oracle (`rust` 298.6 ms vs `sys` 112.7 ms, median of 3 alternating `--quick` `--workloads baseline` rounds) and **2.089** for the boot-dominated `slice` workload's summed per-case wall.
The `foreign`/`list_walk`/`copy_bound`/`gc_bound` **startup-cancelled marginals** still cancel whatever startup a sample pays (`(mean wall@N − mean wall@0) / N`), but every Rust sample now carries the ≈ 0.35 s boot, so those rows measure the loop against a far larger whole-process wall than the dated snapshots record.

## The boot path's one live-but-idle port, and the poll budget
Directive 18 of the 22 in `bootstrap/bin/start.boot` starts the `kernel` application, so `kernel_sup`'s `inet_db` child runs `inet_config:init/0` (`lib/kernel/src/kernel.erl:211-216`, `lib/kernel/src/inet_db.erl:121-123`): it opens a UDP socket, asks `prim_inet` for the hostname, and — when that short name carries no dot — resolves it through `inet_gethost_native`, which opens the helper `{spawn_executable, <erts>/bin/inet_gethost}` port with `{packet,4}` (`lib/kernel/src/inet_gethost_native.erl:413-429`).
That is the oracle's own behaviour (`strace -f` of `erl -noshell -eval 'halt().'` shows the same `execve(…/erts-17.1/bin/inet_gethost)`), so the lookup is fidelity rather than overhead, and it is why a booted run reads **98** `.beam` files where the pre-directive-18 boot reads 97 — the 98th is `inet_gethost_native`.
The helper answers its one framed request in ≈ 2.5 ms and then stays **live but idle** for the rest of the run (`inet_gethost_native` passes no `exit_status`, so no round reaps or closes it — [`src/port/CONTEXT.md`](src/port/CONTEXT.md) *Known divergence from the oracle*), which keeps `PortRegistry::has_nothing_to_poll()` false from directive 18 on.

**The rule: a blocking port poll is justified only while a port owes a reply.**
`PortRegistry::awaits_port_reply()` (`src/port/info_extras.rs`) is exactly that question, read off the per-port byte counters `port_info(Port, input)`/`output` already report: a port with `output > input` was written to and has not answered, while a port that has answered every byte it was given cannot speak again until this runtime writes to it, so waiting on it delivers nothing.
The boot drive is the **one** caller that takes the gate: a `TimerWait::ImminentWithin(_)` pass hands `deliver_port_messages` a `Duration::ZERO` poll when no port owes a reply (`Runtime::awaits_port_reply`, `src/runtime/scheduler.rs`), while the `run` and `-eval` routes keep their unconditional blocking poll because a child on those routes may speak unprompted.
A port table that cannot be borrowed reports *awaiting*, so the gate can only ever shorten a sleep it can justify.
The gate removes sleep, never work: the CPU medians below are identical on both sides of it.

Paired medians of 12 alternating rounds (`taskset -c 3`, `getrusage(RUSAGE_CHILDREN)` deltas, `beam_rt -noshell -eval 'halt().'`, 1-min `/proc/loadavg` 19.95 after the run), gate off vs gate on, against the m15-era release tree as the reference side:

| side | cpu_med (ms) | wall_med (ms) | wall vs m15-era |
|---|---|---|---|
| m15-era tree (`a34ca61332`) | 344.7 | 354.9 | 1.000 |
| this tree, blocking poll on the idle port | 328.5 | 471.9 | 1.330 |
| this tree, gate on | 328.5 | 340.5 | **0.959** |

The oracle side is not part of that table — it boots its own release's script — so the cross-runtime figure is recorded here, through the bench driver with the same binary pair (`BEAM_RT` selects the side), 3 alternating `--quick` `--workloads baseline` rounds under `taskset -c 0-3` (`startup_ms` p50 per round, median of the round p50s; 1-min `/proc/loadavg` 24.96 before / 18.78 after):
| side | `startup_ms` p50 rounds (ms) | median of rounds (ms) |
|---|---|---|
| this tree, gate **off** | 421.7, 418.6, 445.7 | 421.7 |
| this tree, gate **on** | 296.1, 298.6, 298.7 | **298.6** |
| oracle (OTP 29.1.1 / erts-17.1) | 112.7, 127.4, 109.2 | 112.7 |

So gate off → on is **0.708×** (the regression the gate removes is 29 % of the whole-process wall) and the oracle ratio goes **3.741× → 2.649×**.
The boot-dominated `slice` workload (one call per process, so its per-case wall is almost entirely boot) confirms it: summed per-case `_wall_ms`, median of 2 rounds, same hosts and binary pair — gate off **3880.2 ms**, gate on **2625.2 ms**, oracle **1256.6 ms**, i.e. **0.677×** off → on and **3.088× → 2.089×** against the oracle; every one of the nine cases moves by 0.64–0.74× (e.g. `case_fib_wall_ms` 431.8 → 279.0 for 140.6 on the oracle).

Deterministic counters (`strace -f -c -w <bin> -noshell -eval 'halt().'`), gate off → gate on: `epoll_wait` total **130.8 ms → 4.1 ms** over the same **15** calls (the five 25 ms timeouts are exactly what the gate removes: the 2 + 2 + 1 zero-slice boot-drive steps of directives 18/19/20), `read` total 233 ms → 104 ms, `execve` **2** either way (the helper is still spawned exactly once), `clone3` **1**, exit code **0**, stdout and stderr **empty**, and the boot still reads **98** `.beam` files.
The lookup's own wake is not what the gate touches: a reply still in flight keeps `output > input`, so the poll that delivers it blocks exactly as before.

**Caveats.** The host is shared and oversubscribed: read the **ratio** (0.959) and the deterministic counters, never the absolute ms, and do not compare these wall figures with the absolute figures elsewhere in this file — each belongs to its own load window (this one ran at 1-min loadavg ≈ 20).
The `getrusage(RUSAGE_CHILDREN)` split is what makes the claim testable: the wall median moves while the CPU median does not, so the delta is removed sleep rather than removed work.
A reply slower than one poll slice already folds to the typed `blocked()` gap, exactly as it does with the gate.

### Reproduction
```sh
# build the release binaries the two recipes drive
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p beam_rt -p bench

# the deterministic counters (epoll_wait total and call count, read, execve, clone3, exit status)
strace -f -c -w erts_rust/target/release/beam_rt -noshell -eval 'halt().'

# the wall/CPU split, and the boot's `.beam` opens, through the bench driver
erts_rust/target/release/bench run --quick --runtime rust --workloads baseline
```
`bench` inherits the environment, so the same run under `taskset -c 3` and with the load average recorded before and after each round is the paired comparison the table above reports; its raw reports are ephemeral (re-run the recipes to regenerate), and the driver's own methodology is [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md).

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
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p bench -p beam_rt
W=baseline,slice,foreign,list_walk,copy_bound,gc_bound
for i in 1 2 3; do
  erts_rust/target/release/bench run --full --runtime all --workloads $W --out-file $TMPDIR/perf-now/full$i.json
  erts_rust/target/release/bench compare $TMPDIR/perf-now/full$i.json   # rust/sys within one report
  BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --full --runtime all --workloads $W \
    --out-file $TMPDIR/perf-now/interp$i.json                            # the tier-attribution pass
done
```

`bench` spawns the runtime with an **inherited environment**, so exporting `BEAM_RT_TIER=interpreter` on the driver reaches every spawned `beam_rt` (the same mechanism `AFTER` uses); the raw harness JSON under `$TMPDIR/perf-now/` is **ephemeral** (re-run to regenerate).

#### Host-load caveat (read before quoting any absolute number)

This pass ran in a **light window** of the same shared host: the 1-min `/proc/loadavg` stayed at **≈ 0.8–14** (mostly below 8) against `BEFORE`/`AFTER`'s ≈ 20–55, so its **absolute** latencies are lower than those snapshots' for that reason alone (`sys` startup `min` reads **98.64 ms** against `AFTER`'s 160.47 ms although the `sys` runtime did not change). Only the **ratios** and the **minima** are signal; the driver is runtime-major within one report, so the mitigation is again three **block-interleaved** passes (`sys,rust | sys,rust | sys,rust`).

### Current figures (`rust` / `sys`, min of 3, `--full` focused cross-runtime pass)

Same reading convention as `BEFORE`/`AFTER` in [`PERF_HISTORY.md`](PERF_HISTORY.md) (**< 1 ⇒ Rust cheaper, > 1 ⇒ Rust dearer**; the ratio is the headline).
`sys` is the min-of-3 over the default set (the oracle does not see the tier); both `rust` ratios are the min-of-3 of their own set, and the absolute `rust` values of the two sets are the next section's columns.
`slice` is per-case **whole-process** wall (one call per process); the `foreign`/`list_walk`/`copy_bound`/`gc_bound` rows are **startup-cancelled marginals** (`(mean wall@N − mean wall@0) / N`), the interpreter-throughput signal.
This snapshot's `baseline`/`slice` figures are its own preloaded-only-slice measurements, so they are not the boot-paying whole-process wall of *Current startup regime*.

| workload | case | metric (unit) | **rust/sys default (min3)** | range | rust/sys INTERP (min3) | sys (min3) |
|---|---|---|---|---|---|---|
| `baseline` | — | `startup_ms` (ms) | **0.141** | 0.141–0.168 | 0.144 | 98.64 |
| `slice` | `add` | `wall_ms` (ms) | **0.104** | 0.104–0.139 | 0.117 | 110.61 |
| `slice` | `fib` | `wall_ms` (ms) | **0.107** | 0.107–0.141 | 0.134 | 110.34 |
| `slice` | `pick` | `wall_ms` (ms) | **0.120** | 0.120–0.132 | 0.120 | 107.55 |
| `slice` | `calendar_last_day` | `wall_ms` (ms) | **0.124** | 0.124–0.154 | 0.120 | 112.71 |
| `slice` | `queue_from_size` | `wall_ms` (ms) | **0.185** | 0.185–0.197 | 0.137 | 107.66 |
| `slice` | `gb_trees_size` | `wall_ms` (ms) | **0.124** | 0.124–0.151 | 0.126 | 113.64 |
| `slice` | `base64_encode_size` | `wall_ms` (ms) | **0.118** | 0.118–0.145 | 0.138 | 109.70 |
| `slice` | `ordsets_from_size` | `wall_ms` (ms) | **0.124** | 0.124–0.133 | 0.130 | 117.64 |
| `slice` | `string_str_length` | `wall_ms` (ms) | **0.297** | 0.297–0.333 | 0.238 | 113.40 |
| `foreign` | `cowboy_bstr:to_lower/1` | `ns_per_call` (ns) | **46.75** | 46.755–48.989 | 55.57 | 343.00 |
| `foreign` | `cowboy_bstr:capitalize_token/1` | `ns_per_call` (ns) | **64.46** | 64.463–68.124 | 76.29 | 304.17 |
| `foreign` | `emqx_base62:encode/1` | `ns_per_call` (ns) | **41.48** | 41.484–44.988 | 59.43 | 190.51 |
| `foreign` | `emqx_base62:decode/1` | `ns_per_call` (ns) | **49.74** | 49.736–52.828 | 70.23 | 289.88 |
| `list_walk` | — | `ns_per_element` (ns) | **58.14** | 58.145–60.106 | 63.74 | 2.45 |
| `copy_bound` | `etf_roundtrip` | `ns_per_op` (ns) | **109.05** | 109.046–113.693 | 111.01 | 650.76 |
| `copy_bound` | `ets_copy` | `ns_per_op` (ns) | **13.95** | 13.952–14.181 | 14.28 | 222.17 |
| `copy_bound` | `send_recv` | `ns_per_op` (ns) | **145.51** | 145.511–150.696 | 101.65 | 1 455.51 |
| `gc_bound` | `churn` | `ns_per_op` (ns) | **29.90** | 29.904–30.194 | 55.18 | 4 283.39 |

**Reading.** The per-unit interpreter cost is far higher under Rust (**13.9×–145.5×** across the startup-cancelled marginals), and `slice` peak RSS is the one like-for-like cross-runtime memory row (one call per process): `sys` **71.57 MB**, `rust` **27.48 MB** → rust **2.60× lower**.
A loop workload's rust peak RSS is **not** comparable across runtimes — each shape's growth is its own ([`src/runner/MEASURED.md`](src/runner/MEASURED.md) *Growth-shaped loops*).
The `foreign` per-call gap (**41×–64×**) sits in `AFTER`'s band (**37×–55×**), this default tiering the *whole* process.

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

**Reading the tier effect.** The baseline JIT is a clear win for **`foreign`** (**0.73×–0.87×**) and **`gc_bound churn`** (**0.539×**), and neutral for `baseline`, `list_walk` and most `slice` cases — where a preloaded-only one-shot's wall is dominated by `beam_rt` startup, not the interpreted loop.
The only real regressions are the **module-graph-heavy one-shot** rows, `slice`'s `string_str_length` (**1.295×**), `queue_from_size` (**1.217×**) and `copy_bound`'s `send_recv` (**1.456×**): the first-entry compile cost the section below characterises.
`fib`/`base64_encode_size`/`ordsets_from_size` read *better* on the default column, but by less than the per-pass spread (the `slice` range column above is up to ≈35 %), so they are not attributed to the tier.

### The one-shot cold-start cost — current characterisation

The tier's cost is a **one-shot, first-entry compile** that a short-lived process cannot amortise.
The engine's baseline driver compiles **lazily per function** (on first entry of a `(func, pc)`, one sealed mapping per function; the eager whole-module `JitBackend::compile` is retained only for the whole-module introspection surface and `jit_bench`), so the regression a module-graph-heavy one-shot process pays is **small**: it is the gap between the two columns of the tier table above, `slice`'s `string_str_length` **35.34 → 27.28 ms** (**1.295×**) and `queue_from_size` **20.80 → 17.09 ms** (**1.217×**).
`crates/beam_engine/MEASURED.md` records the same behaviour independently for the `string_str_length` shape — the `beam_rt run <bslice_string.beam> bslice_string:str_length/1 '"hello"'` repro, **N=30 interleaved samples**: default (baseline-JIT) **min 31.86 ms** vs `BEAM_RT_TIER=interpreter` **min 25.51 ms** → **≈ 1.22–1.25×**, and the equivalent `tools/bench` `slice` workload reads **31.97 ms default / 31.92 ms interpreter** (min-of-3).
The point estimate depends on host load and on how many of the *entered* functions the process actually runs, so read **≈ 1.2–1.3×** for this shape, not one number.

**`jit_bench` caveat.** `beam_engine`'s `jit_bench` drives the **eager** whole-module artifact only (`JitBackend::compile`), so it pins the eager/hot path and does **not** capture the lazy per-entry path.
The lazy path is measured by the focused cross-runtime pass above and by `crates/beam_engine/MEASURED.md`'s cold-start section, never by `jit_bench`.

The shape of what remains:

- It is a **per-`beam_rt`-OS-process** cold start, not a per-call or a per-process cost: the compiled-function cache is one `thread_local!` per OS thread, shared by every process of one runtime, never across OS processes, so a long-running node compiles a given function **once** and every later process on it reuses the artifact.
- `tools/bench/src/workloads/slice.rs`'s `execute_case` spawns a **new OS process per sample** (`runs_for(Full) = 25`) and `slice` is scored as **whole-process wall**, so every sample pays the first-entry compile of exactly the functions it enters — which is why the module-graph-heavy `slice` rows are the ones that move.
- On the **native** tier a natively linked loop still charges no reduction mid-block (it charges only at a deopt or a heap-admission boundary — see the crate `CONTEXT.md`); on the **interpreter** tier the budget is charged per interpreted instruction, so a non-allocating loop *is* pre-emptible there.
  That is a pre-emption property, not a cost one: the two columns above differ only by the tier.

#### The interpreter path's per-instruction reduction charge — controlled A/B

On the **interpreter** tier the shipped per-instruction charge costs up to **≈ 15 %** of per-call time on the most instruction-bound workload measured here and **≈ 3–4 %** on the other three `foreign` shapes; on the **default baseline-JIT** tier the hook is unreachable mid-block, so this cost is specific to the interpreter path.
Nothing in it changes the pre-emption property the charge exists for (`src/runner/budget.rs`) — it only prices it.

Dated A/B: [`PERF_HISTORY.md`](PERF_HISTORY.md) *The interpreter path's per-instruction reduction charge — controlled A/B* — one HEAD, two variants, five runs per variant, and the command recipe.

### The per-call cost is independent of the iteration count — the per-slice off-heap mirror (`BEFORE` `3aa42c0e41` → `AFTER` `72d778dcf1`)

Every `foreign` case's per-call cost is **flat in `N`**: the engine's `Machine::refc_binaries` roster stays on the reservoir (`RunnerState::reservoir_at`), so no mirror hands the process one slot per payload per slice and no `O(off-heap slots)` scan (`Process::grow_nursery`'s usage figure, `Process::sweep_off_heap_binaries`) sits on the per-call path.
The memory ceiling is the prune's rather than this roster's: the runner's `Yield` arm releases unreachable payloads at the slice boundary, so `erlang:memory(binary)` is flat in the iteration count too.

Dated A/B: [`PERF_HISTORY.md`](PERF_HISTORY.md) *The per-call cost is independent of the iteration count — the per-slice off-heap mirror* — the per-`N` ladder, the paired `bench` A/B, the `sys`-relative multiples and the mechanism.

### The copy-out's learned size hint — `copy_bound/ets_copy` (`BEFORE` `56683d2982` → `AFTER` `ead363d2a5`)

A registered BIF's container result is re-homed onto the process's own scanned young heap, and the copier learns the payload's word count from the first refusal and spends it before the next attempt: `RuntimeContext::copy_out_hint` is `max(hint, copy_shortfall_words(err))`, spent through `hint_grow_gate` into the no-relocation `Process::grow_nursery` seam ahead of the copy, so the successful first attempt is the common path and the sizing walk never runs in the steady state.
The residual is the hint's own scope — it is per `RuntimeContext`, so the first copy of each context still learns by refusing — plus the registry's own copy onto the pooled BIF heap, which is a `beam_bifs` handoff.

Dated A/B: [`PERF_HISTORY.md`](PERF_HISTORY.md) *The copy-out's learned size hint — `copy_bound/ets_copy`* — the refusal cadence, the paired `bench` rows, the `perf` attribution, the memory check and the correctness legs.

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

- [`PERF_HISTORY.md`](PERF_HISTORY.md) → the dated `BEFORE`/`AFTER` snapshots, the delta behind the tier-default and dispatch-prologue changes, and the single-mechanism controlled A/Bs.
- [`PERF_DISPATCH.md`](PERF_DISPATCH.md) → the per-call `DISPATCH-CUT` / `COPY-OUT-CUT` analyses, i.e. the `examples/perf_call.rs` figures with their reproduction recipes.
- [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md) → the bench driver's authoritative environment record, per-target tables, methodology and honesty audit (this file links to it and does not reproduce it). Its `m6` bands are a **different, far more heavily loaded** pass and are not the `CURRENT` baseline.
- [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) → the `perf_bif` harness, the dispatch-cost narrative and the execution-trace seam.
- [`src/runner/MEASURED.md`](src/runner/MEASURED.md) → the per-workload measurement record behind the runner's `Measured` index (growth-shaped loops, the copy-out hint's A/B, the slice budget's charge, the emqx off-heap end-to-end).
- [`src/context/calls/CONTEXT.md`](src/context/calls/CONTEXT.md) · [`src/context/pool/CONTEXT.md`](src/context/pool/CONTEXT.md) → the dispatch path and the BIF-result copy-out this file prices.
- [`CONTEXT.md`](CONTEXT.md) → the runtime crate's contracts and current state.

## The `ets` table-alias witness (the copy-out halved)

`beam_bifs`' `ets` read helpers can hand back the table's own terms (the `TableAliasing` seam) instead of copying every object onto the pooled BIF heap; the host half in `src/context/calls.rs` installs the witness on every dispatch and does the **one** copy in the tail, before the pool rewind.
The structural figure is per op, not a timing: `ets:lookup/2` of one `{k,42}` object writes **5 → 2** words, `tab2list/1` of three objects **15 → 6**, `lookup_element/3` of a boxed element **3 → 0**, and a miss/empty result **0 → 0** (`../../beam_bifs/CONTEXT.md`).
Measured on the `copy_bound/ets_copy` shape (250 000 rust ops, release `beam_rt` vs the pre-change binary kept in `$TMPDIR`, `bench compare` per pair): the **clean** per-op ranges are **2926–3261 ns/op** before and **2382–2624 ns/op** after — a separation of **≥ 11.5 %** with the centres ≈ **−16 %** apart.
The host is shared (21–64 loadavg on 14 cores), so **one of the five pre-registered rounds** passed a strict load filter and only that pair is quoted as a ratio; the rest are retained as ranges, never as a single figure.
Nothing outside the shape moved beyond **± 4 %**: `etf_roundtrip` **+0.5 %**, `list_walk` **−2.2 %**, while `send_recv` and `gc_bound churn` are **inconclusive** at this load.
Read the structural word counts as the result and the −16 % as an indication; re-measure on an idle host before quoting a tighter ratio.

## The NIF-resource prune's two non-park sites (deterministic counters, flat in N)

**Nothing here is a timing.** A NIF resource's payload is a **host `malloc` block** outside every runtime allocator, so no `erlang:memory/1` figure can see the table and no wall clock is separable from it.
Every number below is a **deterministic counter** — the C fixture's own destructor count, its live-object count, and `ResourcePruneStats`' fields — so this section needs no load filter and makes no latency claim; the load context is recorded only for reproducibility.
Host: Linux `x86_64`, `nproc` = **14**, shared — 1-min `/proc/loadavg` **32–47** across the runs (33.79 at the transcription of this section). Both legs are **debug** builds (`erts_rust/scripts/cargo.sh test …`), which is the default for this crate's gates.

**The mechanism.** A resource term owns one reference on its object (`beam_abi::nif::enif::resources`), and *which* terms are still live is only knowable by the host — hence `release_resource_terms(predicate)`.
`src/context/gc/resources.rs::reclaim` is that predicate's caller for a **scheduled** process (the runner's `Yield` park arm, the one point with a published complete root set).
The two **non-park** sites are the new ones, both in `src/eval/reclaim/resource.rs` and both sharing the refc walk's descent through a two-method `Reach` sink (`src/eval/reclaim.rs::walk<R: Reach>`), so there is **one** reachability implementation, not three:
`prune_at_statement_boundary` runs between an `-eval` expression's top-level statements, and `prune_beyond` runs at a **nested run's epilogue** (`src/context/pump.rs::adopt_nested_payloads`, reached by every fun application and cross-module call).
Neither site can enumerate its whole process, so both are gated by a **frontier** — a sorted snapshot of the live resource-term candidate set taken at statement entry / run entry — and release only candidates that appeared **inside** their own window and that their published live set does not name.
That is the same soundness device as `prune_scoped`'s `only` list, and for the same measured reason.

**Matching is two-tier**, because a `copy_struct` copy is a different *word* with the same *identity*: tier 1 is the candidate's own raw word (`binary_search`), tier 2 is `beam_term::boxed::internal_ref_id` on the term's id block. An identity that cannot be read is recorded as **reached** — "cannot identify it" must never become "it is dead".

### The `-eval` statement boundary, N sweep (`tests/nif_resource_gc.rs`)

One runtime, one process; the driver expression is `D0 = nif_resource_gc:dtor_count(), erlang:length([nif_resource_gc:probe()])` ×N, `{dtor_count() - D0, erlang:memory(total)}`.
`erlang:length([probe()])` is load-bearing: it consumes the minted term as a function argument, so the term is genuinely **un-rooted** at the boundary (a bound-but-unused variable would still be a live `Env` value and a release would be a false one).

| leg | mints (cumulative) | dtors | released | prunes | declines | live terms |
|---|---|---:|---:|---:|---:|---|
| prune ON | 10 | 10 | 10 | 10 | 0 | 0 → 0 |
| prune ON | 100 (110) | 100 | 110 | 110 | 0 | 0 → 0 |
| prune ON | 1 000 (1 110) | 1 000 | 1 110 | 1 110 | 0 | 0 → 0 |
| prune OFF | 10 | 0 | 0 | 0 | 0 | 0 → 10 |
| prune OFF | 100 | 0 | 0 | 0 | 0 | 10 → 110 |
| prune OFF | 1 000 | 0 | 0 | 0 | 0 | 110 → 1 110 |

**ON is exactly one destructor per minted-and-dropped term at every N, and the table does not grow by a single entry**; **OFF is exactly linear**, one entry per mint — the pre-fix shape, and the reason the OFF leg exists as a control rather than as a comment.
`released == prunes == ΣN` is one walk per minting **statement** (not per mint), and `declines == 0` says no walk was refused, so the flat half is not a decline in disguise.
The zero residual is real here rather than the documented one-statement leak: the run's very first boundary follows `init/1`, which mints nothing, so the frontier is already correct at the first mint.
The A/B procedure is a **one-line flip** (`ResourcePruneState::default`'s `enabled: true → false`) plus a rebuild; with it flipped the cadence test fails at its first assertion (`left: 0, right: 10`), which is the revert evidence.

### The nested-run epilogue (`tests/nif_resource_nested.rs`)

Every probe is compared against the **system `erl`** on the identical `.beam`s, the identical `.so` and the identical expression text (OTP 29.1.1 / erts-17.1), so the table is an agreement, not a hard-coded expectation.

| probe | prune ON | prune OFF | oracle |
|---|---|---|---|
| `dropped(10)` | `{10,0}` | `{0,10}` | `{10,0}` |
| `dropped(200)` | `{200,0}` | `{0,200}` | `{200,0}` |
| `mapped(10)` / `mapped(200)` | `{10,0}` / `{200,0}` | — | `{10,0}` / `{200,0}` |
| `held(16)` (linear control) | `{16,0}` | — | `{16,0}` |
| `keep_var` / `keep_tuple` / `keep_list` | `{8,0}` | — | `{8,0}` |
| `plain` (probe discriminates) | `{error,8,0}` | — | `{error,8,0}` |

`dropped(N)` is `{dtor_count, live_count}`, so flat in N means the destructor count tracks N while the live-object count returns to `0`; OFF is linear in the live count with no destructor at all.
`mapped(N)` is the **scope** half — the `lists:map/2` shape that pinned the refc version: every element was minted by an *earlier* nested run than the one that reads it, and all of them still resolve through `enif_get_resource` (`probe/0` writes a `42` marker and `peek/1` answers `8` from `enif_sizeof_resource`, so a recycled block at the right address is refused rather than trusted).
`held(16)` is the **non-vacuity** control: the same loop through `probe_held/0`, whose objects the NIF's own C reference keeps alive, is linear — so a broken live-object counter could not make the flat half pass silently.

**Two honesty notes.** (1) The oracle defers a resource `dtor` to misc aux work, so its count only moves after a GC *and* a thread-progress point; the fixture's `dropped/2` does both (`erlang:garbage_collect/0` + `timer:sleep/1`), and because the oracle can still lag **strictly** behind under a loaded host, the dropped legs compare exactly first and otherwise only the shared shape `dtor + live == N`. (2) This runtime releases **synchronously** at each run's epilogue, so the same expression's destructor runs earlier — a timing difference, not a correctness one.

### Cost, and the two device decisions worth recording

**The O(1) steady state is one snapshot and one `Vec` compare per site.** `Frontier::snapshot()` reads `resource_terms()` into a sorted word list (an empty registry allocates nothing); `prune_beyond` then defers when the total word count is unchanged, when nothing is fresh, or when the kill switch is off — so a site with no NIF resource traffic costs one registry read and no walk at all, which is what makes the per-lambda-application price acceptable.
The nested epilogue goes further: when **neither** store is due (neither the refc payload store nor the resource table), it does not copy the root set either.
The park arm's host-seed collection (`Runtime::gather_off_heap_roots`) sits behind gate 1 (`candidates.is_empty()`), so a runtime that never minted a resource pays nothing for it.

**There is no region word scan for resources, and that is a decision, not an omission.** The refc prune gets a region half for free because a payload *pointer* is a value written beside its header, so a `HEADER_BIN_REF` pattern scan finds it without dereferencing anything.
A resource's identity instead lives **inside** its 4-word `[hdr][node][serial][id]` block, and the mint block itself stands in a runtime region (the pooled BIF heap) — so a word-pattern scan would match every candidate every time and make the prune a permanent no-op.
Omitting it is sound for a specific reason: this descent is region-**agnostic** (it never consults the collector's scanned bounds), so every *live* copy of a resource term is reached from the named roots, while a copy sitting in a region nothing names is dead data — regions are never reset, so treating region words as roots would retain forever, which is the "false positive can only retain" argument pointing the other way.
The residual is the one the park arm already documents: a resource held only by a store outside the walk's root families (another process's heap, an ETS table, `persistent_term`) is retained.

### Reproduction

```text
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test nif_resource_gc
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test nif_resource_gc -- --ignored   # the control leg, own process
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test nif_resource_nested
```

The control leg must run in its **own process**: the ABI resource registry is process-global and `Registry::bind_term` evicts a binding whose word a new mint reuses, so a second `Runtime` in the same OS process mints at the same words and the frontier's candidate set is invariant (which would make every prune defer). Both the cadence test and the control carry a loud-skip guard for a dirty registry for exactly that reason.

### See Also

- [`src/eval/reclaim/resource.rs`](src/eval/reclaim/resource.rs) → the frontier, the two-tier sink, the three gates and the in-module unit tests.
- [`src/context/gc/resources.rs`](src/context/gc/resources.rs) → the scheduled-site contract, its five decline gates and its residual class (this section extends it, it does not replace it).
- [`M1_GAPS_A3_NIF.md`](M1_GAPS_A3_NIF.md) *NIF (A.3)* → the residual ledger: the first-boundary statement leak, the nested `Err` path, the pointer-reuse heuristic and the root-family completeness dependency.
## The `-eval` statement window (deterministic word counts, not timings)
**Nothing here is a timing**: every figure is a **word count** from `Runtime::eval_region_stats()` (`Runtime::eval_heap`'s `used`/`capacity` plus the window's counters), deterministic in the expression text, so no load filter applies — the load context is recorded for reproducibility only.
Host: Linux `x86_64`, `nproc` **14**, shared, 1-min `/proc/loadavg` **2.3–4.6** across the runs; both legs are **debug** builds (this crate's gate default).
**The growth this bounds.** `Runtime::eval_heap` is a pointer-stable `beam_alloc::Heap` whose frontier only moves forward; `Runtime::grow_eval_heap` doubles its capacity and pushes the replaced region onto `Runtime::eval_heap_retired`, **never freed**. A run's container results are copied onto it (`src/context/pool.rs::move_result_into_runtime_store`), so without a rewind a run's store is the **total volume it ever copied**, not its live set.
`src/eval/scratch.rs` rewinds the dead prefix at the boundary **before** every top-level statement — the one instant a comma sequence's live set is exactly the evaluator's `Env` plus whatever a runtime-lifetime store kept.
**The reproduction** is `discarded_builds(n)` (`src/eval/scratch/tests.rs`): `n` statements of `_ = lists:seq(1,2000), ` then `ok`.
With the window off (the pre-fix behaviour, default **16 777 216**-word region, **0** retired regions) one `lists:seq(1,5000)` costs **10 000** words and two cost **20 000**; two `binary:copy(<<1>>,40000)` cost the pre-window note's **10 004**.
| `n` | ON `used` | ON `resets` | ON `words_reclaimed` | OFF `used` |
|---:|---:|---:|---:|---:|
| 1 | 0 | 1 | 4 000 | 4 000 |
| 10 | 0 | 10 | 40 000 | 40 000 |
| 100 | 0 | 100 | 400 000 | 400 000 |
| 1 000 | 0 | 1 000 | 4 000 000 | 4 000 000 |
`declines` is **0** on every leg. **ON is flat at the baseline** (each statement's 4 000-word copy is reclaimed by the next boundary); **OFF is exactly `4 000 × n`**, the pre-fix shape — hence the OFF control.
**The cost.** A boundary is one `Option` test plus one `usize` subtraction while the window is closed, pinned, or below `MIN_WINDOW_WORDS` (**1 024** words = 8 KiB); only a window worth reclaiming pays the reachability descent, which stops at its **first hit** (`Reach::done`). Every decline retains every word — a bounded leak of one statement, never a dangle.
**The oracle cross-check** is [`tests/eval_region.rs`](tests/eval_region.rs): a `persistent_term` value, an environment binding and a mailbox message each survive **20** interposed builds, answering `6003000`/`6003000`/`2001000` on both runtimes.
### Reproduction
```text
C="erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt"
$C --lib -- eval::scratch                                                     # the 7 pins
$C --lib -- eval::scratch::tests::the_growth_figures --ignored --nocapture     # the figures above
$C --test eval_region                                                         # the oracle cross-check
```
## `process_info(self(), binary)` — the in-machine roster (deterministic counts, not timings)
**Nothing here is a timing**: the figures are the canonical **integer lists** the query answers, deterministic in the fixture text, so no load filter applies.
The item lists a process's off-heap reference-counted payloads as `[{Ptr, Size, Refc}, …]`; `Ptr` is an address, so two runtimes can agree on the shape, the byte length and the refcount, never on the number.
The suite reduces each answer to `[Count, IsPositiveInt?, Size, Refc, …]` and asserts the raw 3-tuple shape (the item is the 2-tuple `{binary, …}`, every element an integer, the pointer positive) **before** reducing it, so a wrong shape cannot slip through as an empty list.
| query | oracle (system `erl`, OTP 29 / `erts-17.1`) | `beam_rt` |
|---|---|---|
| self, one 40 000-byte off-heap binary built earlier in the slice | `[1,1,40000,1,40000]` | same |
| self, nothing built | `[0]` | same |
| another pid, parked holding one | `[1,1,40000,1]` | same |
The **third** row is the unchanged route: `src/native/memory/mod.rs::snapshot` reads `RunnerState::reservoir_at`, exactly as before, because a scheduler pass takes the running process's engine out of the runner state for the whole slice.
The first two rows are the reading that needed the engine's in-machine BIF door: the published window carries the reservoir too (`src/context/position.rs::in_machine_off_heap_binaries`, `InMachinePosition::off_heap_binaries: Option<Vec<(usize, usize, usize)>>`), and the snapshot takes it as the reservoir half when it names the queried pid.
One `binary_value` builder serves both halves, so the two routes cannot drift.
The reading is gated on the query naming `binary`, so no other `process_info` item pays the roster copy.
### Reproduction
```text
C="erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt"
$C --test process_info_binary        # the three legs, each oracle-cross-checked; no `erl` ⇒ a loud skip
```