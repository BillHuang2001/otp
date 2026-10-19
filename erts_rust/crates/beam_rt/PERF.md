# `beam_rt` — current performance figures (`CURRENT`)

The present-tree measurement record for the Rust runtime: what `beam_rt` costs today on both sides of the comparison, with every figure's qualification attached.
Every number in this file is **measured, never modelled**: nothing here is fabricated, and a sys-only workload that cannot produce a Rust-side figure is recorded as `skipped(<reason>)` with no number.
It is **not** a change log — [`../../../CHANGE.md`](../../../CHANGE.md) records what changed, this file records what it cost.
This file owns the **`CURRENT` snapshot**, the standing *Current startup regime* note that qualifies every `startup_ms` figure in the crate, and the boot path's poll-budget note (*The boot path's one live-but-idle port, and the poll budget*).
The rest of the record lives in two siblings: [`PERF_HISTORY.md`](PERF_HISTORY.md) holds the dated **`BEFORE` → `AFTER`** snapshots (the historical baselines and the delta between them), and [`PERF_DISPATCH.md`](PERF_DISPATCH.md) holds the per-call **`DISPATCH-CUT`** / **`COPY-OUT-CUT`** analyses.
Route by question: today's cross-runtime figures and the execution-tier effect → this file; a dated baseline or a whole-suite `BEFORE`→`AFTER` snapshot → `PERF_HISTORY.md`; a **controlled A/B at one change**, carrying its own two HEADs → this file's `CURRENT` region (the per-instruction reduction charge, and the per-call cost's independence from `N`); registered-BIF per-call cost, the dispatch prologue or the BIF-result copy-out → `PERF_DISPATCH.md`.
The measurement narrative lives in [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md); the bench driver's own environment record, per-target tables and methodology are [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md), which this file links to and does not reproduce.

## Current startup regime (read before quoting any `startup_ms` figure)

Every CLI action except `-version` now drives a boot script before the action's own start instruction: the explicit `-boot FILE` value when given, else **this runtime's own** release's committed `bootstrap/bin/start.boot` (`CodeServer::own_bootstrap_boot_script`; the literal `start` resolves to `<Root>/bin/start.boot` only when the repo bootstrap is undiscoverable, e.g. an installed deployment).
A bare `-eval 'ok.'` invocation therefore boots `start.boot` and is measured at **≈ 0.35 s wall** on this host: paired medians of 12 alternating `taskset -c 3` rounds read **340.5 ms wall over 328.5 ms CPU** for `beam_rt -noshell -eval 'halt().'` (1-min `/proc/loadavg` ≈ 20 after the last round), i.e. the shape is CPU work plus whatever the shared host adds — absolute ms are host-dependent, and *The boot path's one live-but-idle port, and the poll budget* below carries that measurement in full.
Every `startup_ms` figure in this file and in [`PERF_HISTORY.md`](PERF_HISTORY.md) is **that snapshot's own** preloaded-only-slice measurement (the ≈ 14–18 ms band) and its ratio to the oracle is that snapshot's; those numbers do not describe the current whole-process wall.
The present whole-process wall is **CPU-bound and carries no fixed waits**: `strace -f -c -w` over a whole run shows **0 `clock_nanosleep`** calls and **≈ 4 ms** of `epoll_wait` in total, so a blocked round returns instead of sleeping and the residual wall is engine work (the oracle's own `erl -noshell -eval 'halt(0).'` boot is ≈ 0.12–0.15 s here).
The wait a *blocked* pass pays is mechanism-agnostic, so read it as a property — bounded, never a spin — not as one syscall name: with a port live in the table (a booted kernel keeps the `inet_gethost` helper child open, which makes `PortRegistry::has_nothing_to_poll()` false from directive 18 on) the wait is that port's poll, its blocking form is taken only while a port **owes a reply**, and only by a round that ran **zero** slices — the rule and its measured figures are in *The boot path's one live-but-idle port, and the poll budget* below.
Because that wall is this runtime's own CPU cost rather than a fixed sleep, the cross-runtime `startup_ms` ratio is **recorded at the head of *The boot path's one live-but-idle port, and the poll budget*** below: **2.649** against the oracle (`rust` 298.6 ms vs `sys` 112.7 ms, median of 3 alternating `--quick` `--workloads baseline` rounds), and **2.089** for the boot-dominated `slice` workload's summed per-case wall. Every other `startup` ratio in this file and in [`PERF_HISTORY.md`](PERF_HISTORY.md) belongs to its own dated snapshot.
The `foreign`/`list_walk`/`copy_bound`/`gc_bound` **startup-cancelled marginals** still cancel whatever startup a sample pays (`(mean wall@N − mean wall@0) / N`), but every Rust sample now carries the ≈ 0.35 s boot, so those rows measure the loop against a far larger whole-process wall than the dated snapshots in [`PERF_HISTORY.md`](PERF_HISTORY.md) record.

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
→ rust **2.60× lower** (essentially unchanged from `AFTER`'s 2.46×); a loop workload's rust peak RSS
is **not** comparable across runtimes — each shape's growth is its own, and only a shape measured at its
own `N` says anything ([`src/runner/CONTEXT.md`](src/runner/CONTEXT.md) *Measured*). The `foreign`
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
current runtime pays the ≈ 0.35 s boot-script drive on every action, see *Current startup regime*), not
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
  this snapshot's preloaded-only slice; the current runtime instead pays the ≈ 0.35 s boot-script drive,
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

### The per-call cost is independent of the iteration count — the per-slice off-heap mirror (`BEFORE` `3aa42c0e41` → `AFTER` `72d778dcf1`)

**Sides.** `AFTER` is `beam_rt` built from the committed tree at `72d778dcf1`; `BEFORE` is the **same harness** built from `3aa42c0e41`, the commit immediately before it, so this A/B isolates one mechanism and nothing else.
It is, like the reduction-charge A/B above, a controlled A/B carrying its own two HEADs rather than a re-run of the `CURRENT` snapshot's `--full` pass.
The system side (`sys`) is `erl` on `PATH` — **OTP 29.1.1 / erts-17.1** — and did not change between the two sides.
Host: Linux `x86_64`, `nproc` = **14**, **shared and heavily loaded** — the 1-min `/proc/loadavg` read **18–89** during these runs — so **ratios and the per-`N` shape are the evidence, never the absolute wall time**.
Every measured run was pinned with `taskset -c 3`, and every `/proc/loadavg` reading was recorded **before and after** each run.

**The one mechanism the A/B isolates.** `BEFORE` mirrors the engine's `Machine::refc_binaries` reservoir onto the running process at the end of every slice, and `beam_gc::OffHeap` is a slot-stable, never-compacted slot vector, so that mirror hands out one slot per payload per slice for the life of the process and puts three `O(off-heap slots)` scans on the per-call path — the eager `max_heap_size` usage figure that `Process::grow_nursery`/`ensure_capacity` read, and `Process::sweep_off_heap_binaries` at every rooted collection.
`AFTER` keeps the roster on the reservoir itself (`RunnerState::reservoir_at`, unioned with any payload a host registers on the PCB), so the PCB's off-heap table stays **empty** and all three scans walk an empty vector.

**The ladder** — `(wall − boot_baseline) / N`, the boot baseline being the median of 3 `N = 0` runs; the real `emqx_base62.erl` plus a driver loop, `taskset -c 3 <beam_rt> -pa <dir> run <drv.beam> b62drv:<fn>/1 <N>`; 3 rounds, **alternating `BEFORE`/`AFTER`**; every `N` produced **identical checksums on both sides**, so the two sides do the same work:

| `N` | enc `BEFORE` (ns/call) | enc `AFTER` (ns/call) | B/A | dec `BEFORE` (ns/call) | dec `AFTER` (ns/call) | B/A |
|---:|---:|---:|---:|---:|---:|---:|
| 10 000 | 16 287.5 | 7 371.4 | 2.21× | 4 029.6 | 2 852.8 | 1.41× |
| 20 000 | 30 235.7 | 6 418.3 | 4.71× | 5 088.1 | 4 204.4 | 1.21× |
| 40 000 | 78 509.7 | 6 096.0 | 12.88× | 12 656.3 | 6 377.9 | 1.98× |
| 60 000 | 127 050.9 | 4 684.2 | 27.12× | 16 931.6 | 7 377.0 | 2.30× |
| 120 000 | 377 725.2 | 6 581.7 | 57.39× | 31 904.1 | 7 559.4 | 4.22× |

**Reading the ladder.** `BEFORE`'s per-call cost grows **23.2× over a 12× `N` range** (16 287.5 → 377 725.2 ns/call from `N` = 10 000 to 120 000) — super-linear, which is the per-slice `O(slots)` mirror priced per iteration.
`AFTER` is **flat within a noise band** (4 684.2–7 371.4 ns/call across the same 12× range, with no monotone trend), so the per-call cost is **independent of `N`** and the total is linear in the iteration count.
`decode` has the same shape — **7.9× growth → flat** (4 029.6 → 31 904.1 ns/call under `BEFORE`, 2 852.8–7 559.4 under `AFTER`).

**Paired `bench` A/B** — `bench run --quick --runtime all --workloads foreign`, **3 alternating rounds**, medians of the `rust` rows; the `sys` row is that A/B's own noise floor at **0.88–0.92×**, so only a rust-side move outside that band is the change:

| case | `BEFORE` (ns/call) | `AFTER` (ns/call) | B/A |
|---|---:|---:|---:|
| `emqx_base62_encode` | 138 544.3 | 5 667.6 | 24.44× |
| `emqx_base62_decode` | 20 698.3 | 10 028.7 | 2.06× |
| `bstr_to_lower` | 18 504.0 | 8 607.9 | 2.15× |
| `bstr_capitalize_token` | 21 656.3 | 10 485.8 | 2.07× |

**As a multiple of the system ERTS measured in the same rounds**, the `rust` side moved from **392–563×** to **19.3×** (`encode`), **47–64×** to **23.9×** (`decode`), **37–53×** to **19.6×** (`bstr_to_lower`) and **41–59×** to **24.2×** (`bstr_capitalize_token`).

**The defect was not `encode`-specific.** Any loop whose iteration builds an **off-heap refc binary from compiled code** accumulated into the mirror: a second `bstr_to_lower` ladder reads `BEFORE` **11 905 → 26 024 ns/call** from `N` = 10 000 to 60 000 and `AFTER` **flat in a 9 829–11 119 ns/call band** across the same range.
The three non-`encode` paired rows above therefore carry the same shape — a real `BEFORE` value and a flat `AFTER` one — at their own `N`.

**The A/B moves per-call cost, not memory.** Peak RSS on both sides is the same within the noise band — the roster is a machine-level handle list either way, so the difference was per-call work, never a memory win.

**Correctness of the measured side.** `--lib -- runner::tests` **46 passed / 0 failed**, `--lib -- native::memory` **34 passed / 0 failed**, `--test process_info_items` **4 / 0**, `--test engine_off_heap_transport` **6 / 0**.
The re-pointed path was probed directly — a compiled `<<0:N/unit:8>>` held live by a parked child, then `erlang:process_info(P, binary)` → `{1,true}` — and agrees with the system runtime.
The differential cases `gc/process_info_gc_key_set_is_stable`, `gc/process_info_memory_is_positive_integer` and `system_info/process_info_bulk_returns_list` all report `[AGREE]`; **no corpus case exercises the `binary` item**, which `tools/differential/src/corpus/gc.rs` documents as a deliberate capability gap.

**Where the memory ceiling is bounded.** The reservoir is not an accumulator: the runner's `Yield` arm prunes it at the slice boundary whenever the process's binary virtual heap is over its budget, releasing every payload its just-published roots no longer reach (`src/context/gc/bin_vheap.rs`), so `erlang:memory(binary)` is flat in the iteration count — the figures, the sawtooth envelope, the disabled-site A/B and the retained-payload half are [`CONTEXT.md`](CONTEXT.md) *Known Issues / Intentional M1 gaps*'s off-heap bullet, measured by `tests/off_heap_reclaim.rs`.
The end-to-end effect on a real workload — `emqx_base62:encode/1` on the unmodified EMQX module, `PRE-FIX` `dc63fd7f49` vs `HEAD` vs the oracle, all three legs from the identical `.beam` — is [`src/runner/CONTEXT.md`](src/runner/CONTEXT.md) *Measured*; the mirror A/B above is the per-call half of this machinery, never a memory one.
The residuals are that prune's own (a `Wait` park, a declined gate, an un-pre-empted slice, the `-eval` route) — a leak, never a dangle: [`src/context/CONTEXT.md`](src/context/CONTEXT.md) *The off-heap refc-binary reclamation*.
`Process::sweep_off_heap_binaries` returns before touching the roster when nothing was declared dead, so an empty slot space costs nothing on the per-call path.

### The copy-out's learned size hint — `copy_bound/ets_copy` (`BEFORE` `56683d2982` → `AFTER` `ead363d2a5`)

**Sides.** `AFTER` is `beam_rt` built from the committed tree at `ead363d2a5`; `BEFORE` is the **pristine pre-fix tree** at `56683d2982`, so this A/B isolates the copy-out's growth policy and nothing else. Both sides are driven by the **same `bench` binary** (the harness crate is unchanged across the window). The system side (`sys`) is `erl` on `PATH` — **OTP 29.1.1 / erts-17.1**.
Host: Linux `x86_64`, `nproc` = **14**, shared — 1-min `/proc/loadavg` **2.06 → 1.12** during the paired rounds (an earlier pre-fix baseline at load 18–31 published the same shape: `ets_copy` 4 064 / 4 076 / 4 769 ns/op, median 4 076).

**The one mechanism the A/B isolates.** A registered BIF's container result is re-homed onto the process's own scanned young heap (`src/context/calls.rs::move_result_into_process_heap` → `copy_result_fresh`). The default nursery is 233 words (`beam_gc::DEFAULT_MIN_HEAP_WORDS`) and `Nursery::collapse` re-installs a *demand*-sized space after every collection, so the space is small again on the next op. `BEFORE` learned the payload's size only from a **refusal**: `try_copy_fast` ran out of words, and the copier's preflight then walked the payload five times (three of them hashing — `size_object_bounded_with`'s SipHash path set, `is_acyclic`'s two sets) purely to produce a word count the exhaustion error already carried, before growing and retrying. `AFTER` **learns that count and spends it**: `RuntimeContext::copy_out_hint` is `max(hint, copy_shortfall_words(err))`, and `hint_grow_gate` grows the nursery through the no-relocation `Process::grow_nursery` seam **before** the attempt when `available() < hint + max(hint/4, 32)`, so the successful first attempt is the common path and the sizing walk never runs. `hint == 0` reproduces `BEFORE` exactly, and the whole refusal fallback stays behind it (fresh per-attempt destination table, permanent-non-exhaustion short-circuit, `safe_to_rewind`, and the `max_heap_words` ceiling refusing a headroom request → bare-shortfall fallback).

**Cadence, on the compiled `ets_copy` shape** (250 000 ops, `taskset -c 3`, 3 alternating paired rounds, same binary with the hint disabled vs enabled):

| counter | `BEFORE` | `AFTER` |
|---|---:|---:|
| refusals | 83 067 (**0.332 /op**) | 754 (**0.003 /op**) |
| copy attempts | 1.016 /op | 1.016 /op |
| hint-driven grows | 0 | 83 387 |
| collections /op | 0.33–0.34 (`ensure_heap_full` 83 289, reds door 160) | 0.33–0.34 (`ensure_heap_full` 84 158, reds door 162) |
| mean `available()` at the copy-out | 277 words | 190 words |

Each avoided refusal is worth ≈**4.2 µs**, which is the whole regression.

**Paired `bench` A/B** — `bench run --quick --runtime rust --workloads copy_bound,gc_bound,slice`, **3 rounds, alternating sides per round**, medians:

| case | `BEFORE` (ns) | `AFTER` (ns) | B/A |
|---|---:|---:|---:|
| `copy_bound/ets_copy` | 3 586.5 | **2 171.4** | **0.605×** |
| `copy_bound/etf_roundtrip` | 1 419.5 | 1 405.2 | 0.990× |
| `copy_bound/send_recv` | 8 149.7 | 7 875.5 | 0.966× |
| `copy_bound/send_recv_plain` | 1 886.3 | 1 872.9 | 0.993× |
| `gc_bound/churn` | 65 497.8 | 66 275.0 | 1.012× |

`ets_copy` is **−39.5 %** and now sits at **0.67×** of the m15-era figure (3 229 ns/op) it had regressed *from* — i.e. below the pre-regression floor rather than merely back on it. Every sibling case is inside its own noise band.

**Memory is unchanged.** `case_ets_copy_peak_rss_bytes` 118.72 MB → 118.57 MB (0.999×), `churn` 118.54 → 118.40 MB, `etf_roundtrip` 120.21 → 119.87 MB. The growth is a pre-copy `Nursery::grow`, which retires the space it replaces; a collection still collapses the chain, so no space is retained for longer than it was before.

**`perf` attribution, on the raw shape** (`beam_rt -pa <dir> run probe.beam probe:ets_copy/1 250000`, `perf record -F 500`, same probe on both sides — the wall clock is 1 349 ms `BEFORE` → 877 ms `AFTER`, 0.650×, and 877 ms is the diagnosis's `+hms 200000` floor of 816 ms):

| symbol share | `BEFORE` | `AFTER` |
|---|---:|---:|
| `RandomState::hash_one::<&usize>` | 11.09 % | — (below the 1.2 % cut) |
| `SipHasher13::write` | 5.71 % | 1.59 % |
| `HashMap<usize,()>::insert` | 3.87 % | — |
| `size_object_bounded_with` | 4.19 % | — |
| `sizing_walk` | 1.68 % | — |
| **sizing machinery, total** | **26.5 %** | **~0** |
| `shallow_unshared::<TrialTarget>` (the copier itself) | 7.91 % | 10.16 % |
| `RuntimeContext::call_registered` | 7.91 % | 12.42 % |

The refusal machinery is gone from the profile; the new top-share symbols are the two **copies that remain** — `call_registered` 12.42 % and the copier's `shallow_unshared` 10.16 %, beside `beam_ets::TermStore::{alloc_words,copy_term,release}` (3.47 + 2.98 + 2.49 %) and allocator churn. That is the quantified `beam_bifs` double-copy handoff: the registry's own copy onto the pooled BIF heap is still paid on every `ets:lookup`.

**One shape's cadence did move** (wall time unmoved): `gc_bound/churn`'s loop copies out a 257-word `list_to_tuple/1` result, so its growth now happens *before* the copy; one growth = one retired space = one unit of collection debt, so its schedule shifts from 13 971 collections / 2 997 majors to **15 346 / 2 514** at `N` = 2 000 (7.6728 collections per iteration, one major per iteration plus a fixed 514) against a wall change of 1.012×. The `MEASURED` table in `src/runner/tests/boundary_cost.rs` and `src/runner/CONTEXT.md` are re-derived from it; the door's own invariants (`checks == yields`, `unpublished == 0`, the `checks/9 … checks/7` ration, one young space, `capacity ≤ 610`) all still hold, so this is a table move, not a door regression.

**Correctness of the measured side.** `--lib` **1 520 passed / 0 failed / 3 ignored**; `--lib -- runner` 47 / 0; `--lib -- copy_out` 3 / 0 (including the collecting-default-process case and its no-hint control, which refuses again); `--lib -- context::calls` 14 / 0 (1 ignored); `--test max_heap_enforce` 6 / 0, `--test max_heap_flag` 12 / 0, `--test process_info_items` 2 / 0, `--test engine_off_heap_transport` 4 / 0, `--test port_wake` 1 / 0. `build --release -p beam_rt -p bench --all-targets` is **warning-free**.

**Residual / owned by other nodes.**
The hint is **per `RuntimeContext`**, so the first copy of each context still learns by refusing: **754 refusals per 250 000 ops ≈ 13 ns/op (0.4 %)**. Removing it needs a `Runtime`-owned pid→hint map with cleanup on process death (this crate, next pass).
The `beam_term::size` hashing walk is now **off this shape's critical path** (it was the #1 `perf` symbol at ~8 % while refusals ran at 0.332/op) — what remains of it is that 0.4 %.
The registry's own copy is untouched: every `ets:lookup` result is still copied twice (`beam_bifs::ets::copy_out` onto the pooled BIF heap, then `copy_result_fresh` onto the process heap; `shallow_unshared::<TrialTarget>` was 5.7–8.1 % of the run) — that is a `beam_bifs`/pool-rewind contract change, not expressible here.
A deeper `beam_gc` fix (feed the collapse the post-copy live demand so a no-relocation growth survives a collection) would make the growth permanent instead of per-space, and `+hmax` still does not bound the growth-retired nursery chain.

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
- [`M1_GAPS.md`](M1_GAPS.md) *NIF (A.3)* → the residual ledger: the first-boundary statement leak, the nested `Err` path, the pointer-reuse heuristic and the root-family completeness dependency.