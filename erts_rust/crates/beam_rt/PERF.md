# `beam_rt` — performance snapshot (`BEFORE` → `AFTER` → `CURRENT`)

Three **dated, HEAD-labelled** measurement snapshots of the Rust runtime, each taken with the **same
method** so the behavioural changes that landed between them can be read off directly. `BEFORE` and
`AFTER` are **historical records kept verbatim** — their figures describe the tree at *their* HEAD, not
today's — and `CURRENT` is measured at the HEAD named in its own section and is the latest snapshot to
quote for the present tree (its startup figures excepted — see *Current startup regime*).
This file is **not** a change log and does **not** supersede the tool-specific
records — for the bench driver's full environment record, its per-target tables and its methodology see
[`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md); for the per-call harness and the
dispatch-cost narrative see [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md).

Every number here is **measured, never modelled**. Nothing was fabricated; the sys-only workloads
that cannot produce a Rust-side figure are recorded as `skipped(<reason>)` with no number.

## Current startup regime (read before quoting any `startup_ms` figure)

Every CLI action except `-version` now drives a boot script before the action's own start instruction: the explicit `-boot FILE` value when given, else **this runtime's own** release's committed `bootstrap/bin/start.boot` (`CodeServer::own_bootstrap_boot_script`; the literal `start` resolves to `<Root>/bin/start.boot` only when the repo bootstrap is undiscoverable, e.g. an installed deployment).
A bare `-eval 'ok.'` invocation therefore boots `start.boot` and is measured at **≈ 0.55 s wall** on this host (555/557/563/643 ms over 5 runs at 1-min loadavg ≈ 1.1, so absolute ms are host-dependent).
Every `startup_ms` figure below is **that snapshot's own** preloaded-only-slice measurement (the ≈ 14–18 ms band) and its ratio to the oracle is that snapshot's; those numbers do not describe the current whole-process wall.
The present whole-process wall is **CPU-bound and carries no fixed waits**: `strace -c -f` shows **0 `clock_nanosleep`** calls and only **zero-timeout** `epoll_wait` calls (12 calls, ≈ 6 µs total), so a blocked round returns instead of sleeping and the residual ≈ 0.55 s is engine work (the oracle's own `erl -noshell -eval 'halt(0).'` boot is ≈ 0.12–0.15 s here).
Because that wall is now this runtime's own CPU cost rather than a fixed sleep, **no startup ratio measured against the oracle was re-recorded at this HEAD** — the committed `startup` ratios belong to the dated snapshots below — so the `startup_ms` metric should be **re-recorded at the next benchmark milestone** before any current startup advantage or loss is quoted.
The `foreign`/`list_walk`/`copy_bound`/`gc_bound` **startup-cancelled marginals** still cancel whatever startup a sample pays (`(mean wall@N − mean wall@0) / N`), but every Rust sample now carries the ≈ 0.55 s boot, so those rows measure the loop against a far larger whole-process wall than the snapshots below record.

# `BEFORE` snapshot — HEAD `3f24c5ec9d`

## Method

| item | value |
|---|---|
| HEAD | `3f24c5ec9dff664e19db48ed70e5cd450c0e9e5e` (`3f24c5ec9d`, 2026-10-11) |
| Date (UTC) | 2026-10-11 15:0x–15:35 UTC |
| Host / kernel | Linux 7.2.9 `x86_64`, `nproc` = **14** — a **shared, oversubscribed** host |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl`, **OTP 29.1.1 / erts-17.1** (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/.../bin/erl`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| N | **3** full focused cross-runtime passes; reported figure is the **min over the 3**, with the observed range quoted |
| Host load (1-min `/proc/loadavg`) | pass 1 `34.86 → 27.98`; pass 2 `27.98 → 33.07`; pass 3 `33.07 → 20.35`; `perf_bif` ≈ `21.8 → 22.5`; registered-BIF bracket `26.6 → 54.8` |

### Commands

```sh
# build the runtime and the driver
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p bench -p beam_rt

# 3 x the focused cross-runtime pass (six Runtimes::Both workloads, --full, sys + rust)
for i in 1 2 3; do
  erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file $TMPDIR/perf-before/full$i.json
  erts_rust/target/release/bench compare $TMPDIR/perf-before/full$i.json   # rust/sys within one report
done

# the crate's registered-BIF / dispatch harness (3 x)
erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml -q -p beam_rt --example perf_bif
```

The raw harness JSON + `bench compare` outputs were written under `$TMPDIR/perf-before/` and are
**ephemeral** (not committed — this task adds only this file); re-run the commands above to regenerate.

### Host-load caveat (read before quoting any absolute number)

The benchmark host is shared and other agent builds run concurrently, so the 1-min `/proc/loadavg`
sat at **≈ 20–55** (≈ 1.5–4× `nproc`) during this pass. **Absolute latencies are load-polluted and are
not comparable across runs** — only the **ratios** and the **minima** are signal. The driver is
runtime-major (all `sys` rows, then all `rust` rows) within one report, so the three passes were run
**interleaved at block granularity** (`sys,rust | sys,rust | sys,rust`), which is the mitigation used
here; a strictly interleaved per-workload alternation is not expressible through the CLI (the two-file
`bench compare` matches rows by `(workload,runtime)` and does not cross the runtimes).
Read the figures below as a **`BEFORE` baseline on a loaded host**, not a clean-machine number.

## Current figures (rust / sys, min of 3, `--full` focused cross-runtime pass)

`rust/sys` = Rust cost as a multiple of the system-ERTS cost (**< 1 ⇒ Rust cheaper, > 1 ⇒ Rust dearer**).
The `sys` / `rust` columns are the **per-unit metric** min-of-3 for each side (best case of each runtime),
for context; the ratio is the headline. `slice` is per-case **whole-process** wall (one call per process);
the `foreign`/`list_walk`/`copy_bound`/`gc_bound` rows are **startup-cancelled marginals**
(`(mean wall@N − mean wall@0) / N`), the interpreter-throughput signal.
These `baseline`/`slice` figures are this snapshot's own preloaded-only-slice measurements; every CLI action now pays the boot script (see *Current startup regime*), so they are not the current whole-process wall.

| workload | case | metric (unit) | **rust/sys (min of 3)** | range over 3 | sys (min of 3) | rust (min of 3) |
|---|---|---|---|---|---|---|
| `baseline` | — | `startup_ms` (ms) | **0.126** | 0.126–0.146 | 136.52 | 17.80 |
| `slice` | `add` | `wall_ms` (ms) | **0.114** | 0.114–0.124 | 154.01 | 17.87 |
| `slice` | `fib` | `wall_ms` (ms) | **0.107** | 0.107–0.125 | 150.98 | 17.57 |
| `slice` | `pick` | `wall_ms` (ms) | **0.111** | 0.111–0.119 | 154.36 | 17.25 |
| `slice` | `calendar_last_day` | `wall_ms` (ms) | **0.114** | 0.114–0.123 | 156.04 | 18.17 |
| `slice` | `queue_from_size` | `wall_ms` (ms) | **0.127** | 0.127–0.148 | 154.89 | 21.85 |
| `slice` | `gb_trees_size` | `wall_ms` (ms) | **0.101** | 0.101–0.123 | 148.99 | 18.35 |
| `slice` | `base64_encode_size` | `wall_ms` (ms) | **0.088** | 0.088–0.126 | 155.29 | 18.52 |
| `slice` | `ordsets_from_size` | `wall_ms` (ms) | **0.063** | 0.063–0.112 | 152.26 | 17.02 |
| `slice` | `string_str_length` | `wall_ms` (ms) | **0.120** | 0.120–0.238 | 146.00 | 32.85 |
| `foreign` | `cowboy_bstr:to_lower/1` | `ns_per_call` (ns) | **52.9** | 52.9–55.9 | 392.79 | 21 628.36 |
| `foreign` | `cowboy_bstr:capitalize_token/1` | `ns_per_call` (ns) | **59.9** | 59.9–87.7 | 355.96 | 26 292.49 |
| `foreign` | `emqx_base62:encode/1` | `ns_per_call` (ns) | **52.6** | 52.6–74.1 | 232.86 | 16 870.86 |
| `foreign` | `emqx_base62:decode/1` | `ns_per_call` (ns) | **76.9** | 76.9–85.5 | 328.41 | 26 879.50 |
| `list_walk` | — | `ns_per_element` (ns) | **79.4** | 79.4–144.9 | 2.29 | 312.97 |
| `copy_bound` | `etf_roundtrip` | `ns_per_op` (ns) | **114.9** | 114.9–131.6 | 676.37 | 86 429.37 |
| `copy_bound` | `ets_copy` | `ns_per_op` (ns) | **9.56** | 9.56–13.68 | 254.07 | 3 424.21 |
| `copy_bound` | `send_recv` | `ns_per_op` (ns) | **120.5** | 120.5–123.5 | 1 537.05 | 189 637.25 |
| `gc_bound` | `churn` | `ns_per_op` (ns) | **40.2** | 40.2–46.3 | 4 531.62 | 187 960.84 |

**Reading.** The same two-directional story the bench driver records (see `RESULTS.md` §3.8):
whole-process cost at this HEAD was far lower under Rust (`baseline` startup **0.126×**, `slice` cases
**0.063×–0.238×** — this snapshot's preloaded-only slice, not the boot-paying regime of *Current startup
regime*), while the per-unit interpreter cost is far higher (**9.6×–120.5×** across the
startup-cancelled marginals). The bands sit inside the `RESULTS.md` m6 bands (rust startup 0.14×,
`foreign` 74.9×–131.4×, `list_walk` 108.7×, `copy_bound` 17.5×–188.4×, `gc_bound` 55.6×) — the
rust side reads **lower** here because this host carried a far lighter load than the m6 pass
(≈ 20–55 vs the m6 ≈ 414–440), which is exactly why these figures are a loaded-host baseline.
`slice` peak RSS is the only like-for-like cross-runtime memory row (one call per process); the loop
workloads' rust peak RSS grows with the per-runtime loop count `N` and is **not** comparable
(`RESULTS.md` §6.7).

## Per-call dispatch — `perf_bif` (3 runs, min of 3)

`erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml -q -p beam_rt --example perf_bif`
(two-point slope `(t(N2)−t(N1))/(N2−N1)`, `N2 = 4·N1`, `N1 = 16 000 000`, fastest of 5 after one
discarded warm-up per point):

| workload | ns/call (min of 3) | observations (3 runs) |
|---|---|---|
| `pc:add` (native-inlined `gc_bif` loop) | **4.03** | 4.66 / 4.03 / 4.98 |
| `pc:loop` (native-inlined `gc_bif` loop) | **4.47** | 4.74 / 4.47 / 5.47 |
| marginal `pc:loop − pc:add` | **0.08** | 0.08 / 0.43 / 0.50 |

**`perf_bif`'s fixture does not exercise the registered-BIF path.** Its body lowers `N-1`/`N+1` to
`gc_bif` **instructions** the baseline tier inlines natively, so the figures above are the
**native-loop** cost (~4 ns/iteration), *not* a `call_ext` registered-BIF round trip — the harness's
own banner says so. The **registered-BIF** per-call figure therefore has to be measured separately.

### Registered-BIF (non-inlinable) per-call — supplementary measurement

Same two-point slope methodology, on a fixture whose loop body carries a **non-inlinable** BIF (so it
lowers to `call_ext` and goes through `beam_rt`'s dispatch). Fixtures `nb:bif/2` = `bif(N-1, Acc+trunc(N))`
and `nb2:absb/2` = `absb(N-1, Acc+abs(N))`; the arithmetic-only `nb:arith/2` = `arith(N-1, Acc+N)` is the
inlined baseline. Measured with the runtime's own `run` action, driving points interleaved to cancel
load drift, best-of-5 (bracket best-of-3 at `N1=4M`):

| workload | `ns/call` |
|---|---|
| `nb:arith/2` (inlined `gc_bif` arithmetic baseline) | 5.27 |
| `nb:bif/2` — `erlang:trunc/1` (`call_ext`) | **302.1** (interleaved best-of-5); 313.0 (non-interleaved) |
| `nb2:absb/2` — `erlang:abs/1` (`call_ext`) | **322.3** |

**Recorded prior figure / discrepancy.** `TRACE_AND_PERF.md` records ≈ **116 ns/call** for a
non-inlinable BIF (e.g. `trunc/1`) via this same harness; at this HEAD the non-inlinable BIF path reads
**≈ 3× higher (~300–320 ns/call)**, the two non-inlinable BIFs agreeing with each other, and it is
flagged as the `AFTER` target of this snapshot.

## External-repo workloads (sys-only) — not part of this Rust-side snapshot

`cowboy`, `khepri`, `plb`, `emqx`, `erlc_compiler` and `elixir_compiler` are `Runtimes::SysOnly`: they
measure the **system ERTS** and cannot produce a Rust-side figure, so they are out of scope for a
beam_rt BEFORE baseline and were not run here.

| workload | status | reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | **verified** by a direct run — the checkout pins OTP 28 while the host `erl` is OTP 29 (`RESULTS.md` §4.3). No number. |
| `cowboy` | not run | sys-only; builds a relx release with a network `git` fetch of cowlib/ranch (~67 s, `RESULTS.md` §4.4) — neither quick nor Rust-relevant |
| `khepri` | not run | sys-only; `rebar3 eunit` ≈ 80–100 s (`RESULTS.md` §4.2) — not quick and Rust-irrelevant |
| `plb`, `erlc_compiler`, `elixir_compiler` | not run | sys-only (no Rust side to snapshot) |

# `AFTER` snapshot — HEAD `e01965432c`

## What changed since `BEFORE`

Two behavioural commits landed between the two snapshots (verified: `git log --oneline --no-merges
3f24c5ec9d..HEAD -- erts_rust/` is 8 commits, of which 6 touch only `CONTEXT.md`/`M1_GAPS.md`/comments
or add tests):

1. **The execution tier now defaults to the native baseline JIT for *every* process**
   (`108cc929c4`). `JitMode::Auto ⇒ Tier::BaselineJit` on the first entry, remembered across
   pre-emptions and suspends; `BEAM_RT_TIER=interpreter` is the **only** kill switch. The `BEFORE`
   snapshot was taken with the **old** default — a static per-module *switch-safety predicate* that
   kept a module with an `apply` (or a not-provably-synchronous `call_ext`) on the **interpreter for
   its whole life**. So `BEFORE`'s per-unit figures for those workloads are **interpreter** figures;
   the `BEAM_RT_TIER=interpreter` column below is the closest like-for-like reproduction of the old
   default and is the point that separates the **tier effect** from the **dispatch-prologue effect**.
2. **The registered-BIF dispatch prologue was cut** (`c9d2b9d7fd`): one per-`RuntimeContext`
   dispatch memo replaces ~10 cross-crate `uses_*` binary searches plus up to 2–3 registry lookups per
   external call, the caller module is no longer cloned, and `PendingInterner` no longer
   heap-allocates per call. Measured separately in its own section below.

Read the two snapshots against each other with the tier column in hand: **a row whose `AFTER`
*default* and `AFTER` *interpreter* figures differ is reporting a tier effect; a row where the
*interpreter* figure moved against `BEFORE` is reporting the dispatch-prologue effect.**

## Method (identical to `BEFORE`)

| item | value |
|---|---|
| HEAD | `e01965432c1b33b0bd156993b79ad017e5a5919c` (`e01965432c`, 2026-10-12 00:09 +0800) |
| Date (UTC) | 2026-10-11 16:11–16:40 UTC |
| Host / kernel | Linux 7.2.9 `x86_64`, `nproc` = **14** — the **same shared, oversubscribed** host as `BEFORE` |
| Toolchain | `rustc 1.98.1`, `cargo 1.98.0` |
| Oracle (`sys`) | system `erl`, **OTP 29.1.1 / erts-17.1** (same Nix-store path as `BEFORE`) |
| Rust runtime (`rust`) | `erts_rust/target/release/beam_rt` (release), `erlang:system_info(otp_release)` = `"30"` |
| N | **3** default-mode passes **+ 3** `BEAM_RT_TIER=interpreter` passes; each set reported as the **min over its 3**, with the observed range quoted |
| Host load (1-min `/proc/loadavg`) | default passes `45.63 → 49.71`, `51.57 → 43.49`, `43.49 → 44.72`; interpreter passes `43.50 → 37.26`, `37.26 → 33.93`, `33.93 → 41.45`; `perf_bif` `46.26 → 42.23`; registered-BIF `40.81 → 38.37` (default) / `35.70 → 30.98` (interpreter) |

### Commands

```sh
# build the runtime and the driver
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p bench -p beam_rt

# 3 x the focused cross-runtime pass on the CURRENT default (Auto => baseline JIT)
for i in 1 2 3; do
  erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file $TMPDIR/perf-after/full$i.json
  erts_rust/target/release/bench compare $TMPDIR/perf-after/full$i.json
done

# 3 x the SAME pass with the interpreter pinned — the tier-attribution point
for i in 1 2 3; do
  BEAM_RT_TIER=interpreter erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file $TMPDIR/perf-after/interp$i.json
done

# the crate's registered-BIF / dispatch harness (3 x)
erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml -q -p beam_rt --example perf_bif
```

`bench` spawns the runtime with an **inherited environment** (`proc.rs::run` never calls `env_clear`),
so exporting `BEAM_RT_TIER=interpreter` on the driver reaches every spawned `beam_rt` — that is what
makes the interpreter pass expressible through the unmodified driver.

The raw harness JSON + `bench compare` outputs were written under `$TMPDIR/perf-after/` and are
**ephemeral** (not committed — this task adds only this file); re-run the commands above to
regenerate. The registered-BIF fixtures (`nb.beam`/`nb2.beam`) and their two-point slope script were
built under `$TMPDIR` too — **never in the repo**.

### Host-load caveat (read before quoting any absolute number)

Identical to `BEFORE` in kind: the host is shared and other agent builds ran concurrently, so the
1-min `/proc/loadavg` sat at **≈ 31–52** (≈ 2.2–3.7× `nproc`) across every pass here — a somewhat
**lighter** band than `BEFORE`'s ≈ 20–55 peak but with the same non-monotonic relationship to the
measured figures. **Absolute latencies are load-polluted and are not comparable across passes**; only
the **ratios** and the **minima** are signal, and the driver is runtime-major (all `sys` rows, then
all `rust` rows) within one report, so the mitigation is again the three **block-interleaved** passes
(`sys,rust | sys,rust | sys,rust`). Read the `AFTER` figures as a **loaded-host measurement**, not a
clean-machine number — the same caveat `BEFORE` carries.

## Current figures (`rust` / `sys`, min of 3, `--full` focused cross-runtime pass)

Same reading convention as `BEFORE`. The two ratio columns are the headline: **`AFTER`** is the
current default (baseline JIT for every process), **`INTERP`** pins the old-default-equivalent
interpreter via `BEAM_RT_TIER=interpreter`. Every column is taken from the **same 3 passes** of its
own mode; `sys` is the min-of-6 over both sets (the oracle does not see the tier).

| workload | case | metric | **rust/sys AFTER (min3)** | range | **rust/sys INTERP (min3)** | range | sys (min3) | rust AFTER (min3) | rust INTERP (min3) |
|---|---|---|---|---|---|---|---|---|---|
| `baseline` | — | `startup_ms` (ms) | **0.065** | 0.065–0.133 | 0.103 | 0.103–0.128 | 160.47 | 18.08 | 19.50 |
| `slice` | `add` | `wall_ms` (ms) | **0.063** | 0.063–0.129 | 0.090 | 0.090–0.120 | 169.20 | 17.59 | 17.79 |
| `slice` | `fib` | `wall_ms` (ms) | **0.072** | 0.072–0.134 | 0.100 | 0.100–0.113 | 175.33 | 19.76 | 19.22 |
| `slice` | `pick` | `wall_ms` (ms) | **0.065** | 0.065–0.111 | 0.104 | 0.104–0.114 | 174.08 | 18.83 | 19.51 |
| `slice` | `calendar_last_day` | `wall_ms` (ms) | **0.082** | 0.082–0.110 | 0.110 | 0.110–0.117 | 173.36 | 19.93 | 19.28 |
| `slice` | `queue_from_size` | `wall_ms` (ms) | **0.129** | 0.129–0.216 | 0.123 | 0.123–0.154 | 166.30 | 35.85 | 21.85 |
| `slice` | `gb_trees_size` | `wall_ms` (ms) | **0.070** | 0.070–0.109 | 0.090 | 0.090–0.114 | 178.93 | 20.02 | 19.64 |
| `slice` | `base64_encode_size` | `wall_ms` (ms) | **0.078** | 0.078–0.117 | 0.100 | 0.100–0.124 | 172.95 | 21.08 | 19.89 |
| `slice` | `ordsets_from_size` | `wall_ms` (ms) | **0.086** | 0.086–0.119 | 0.106 | 0.106–0.112 | 172.95 | 19.88 | 19.31 |
| `slice` | `string_str_length` | `wall_ms` (ms) | **1.798** | 1.798–2.167 | 0.170 | 0.170–0.196 | 176.27 | 405.29 | 33.68 |
| `foreign` | `cowboy_bstr:to_lower/1` | `ns_per_call` (ns) | **39.66** | 39.66–44.69 | 55.30 | 55.30–57.52 | 423.07 | 19 263.24 | 23 394.11 |
| `foreign` | `cowboy_bstr:capitalize_token/1` | `ns_per_call` (ns) | **54.81** | 54.81–60.06 | 73.04 | 73.04–79.99 | 387.06 | 24 258.18 | 28 269.01 |
| `foreign` | `emqx_base62:encode/1` | `ns_per_call` (ns) | **37.02** | 37.02–47.65 | 52.55 | 52.55–57.97 | 259.15 | 11 059.06 | 14 041.06 |
| `foreign` | `emqx_base62:decode/1` | `ns_per_call` (ns) | **50.03** | 50.03–59.79 | 58.77 | 58.77–66.53 | 350.61 | 19 718.33 | 22 345.65 |
| `list_walk` | — | `ns_per_element` (ns) | **38.05** | 38.05–63.22 | 30.79 | 30.79–44.05 | 3.36 | 173.63 | 169.23 |
| `copy_bound` | `etf_roundtrip` | `ns_per_op` (ns) | **134.29** | 134.29–185.03 | 130.16 | 130.16–141.70 | 694.13 | 97 329.21 | 94 967.24 |
| `copy_bound` | `ets_copy` | `ns_per_op` (ns) | **13.60** | 13.60–17.96 | 12.68 | 12.68–13.90 | 279.67 | 3 931.94 | 3 869.97 |
| `copy_bound` | `send_recv` | `ns_per_op` (ns) | **150.45** | 150.45–191.91 | 104.31 | 104.31–105.78 | 1 554.29 | 245 275.29 | 168 378.61 |
| `gc_bound` | `churn` | `ns_per_op` (ns) | **30.29** | 30.29–43.42 | 65.25 | 65.25–70.30 | 4 848.01 | 167 147.49 | 341 251.57 |

`slice` peak RSS remains the one like-for-like cross-runtime memory row (one call per process):
`sys` **71.91 MB**, `rust` **29.27 MB** → rust **2.46× lower** (essentially unchanged from `BEFORE`'s
2.65×), and the loop workloads' rust peak RSS still grows with the per-runtime loop count `N`
(`RESULTS.md` §6.7) so those rows are not comparable.

## `rust` BEFORE → AFTER delta

Both `BEFORE` and `AFTER` figures are the **min of 3** of the side's own metric; `< 1×` is an
improvement.

| workload | case (unit) | rust BEFORE | rust AFTER (default) | AFTER/BEFORE | rust AFTER (interpreter) | INTERP/BEFORE |
|---|---|---|---|---|---|---|
| `baseline` | startup (ms) | 17.80 | 18.08 | 1.02× | 19.50 | 1.10× |
| `slice` | `add` (ms) | 17.87 | 17.59 | 0.98× | 17.79 | 1.00× |
| `slice` | `queue_from_size` (ms) | 21.85 | 35.85 | **1.64× worse** | 21.85 | 1.00× |
| `slice` | `string_str_length` (ms) | 32.85 | 405.29 | **12.34× worse** | 33.68 | 1.03× |
| `foreign` | `bstr_to_lower` (ns/call) | 21 628 | 19 263 | **0.89×** | 23 394 | 1.08× |
| `foreign` | `bstr_capitalize_token` (ns/call) | 26 292 | 24 258 | **0.92×** | 28 269 | 1.08× |
| `foreign` | `emqx_base62_encode` (ns/call) | 16 871 | 11 059 | **0.66×** | 14 041 | 0.83× |
| `foreign` | `emqx_base62_decode` (ns/call) | 26 880 | 19 718 | **0.73×** | 22 346 | 0.83× |
| `list_walk` | walk (ns/element) | 312.97 | 173.63 | **0.55×** | 169.23 | **0.54×** |
| `copy_bound` | `etf_roundtrip` (ns/op) | 86 429 | 97 329 | 1.13× | 94 967 | 1.10× |
| `copy_bound` | `ets_copy` (ns/op) | 3 424 | 3 932 | 1.15× | 3 870 | 1.13× |
| `copy_bound` | `send_recv` (ns/op) | 189 637 | 245 275 | **1.29× worse** | 168 379 | 0.89× |
| `gc_bound` | `churn` (ns/op) | 187 961 | 167 147 | **0.89×** | 341 252 | 1.82× |

**Reading the delta.** The two landed changes pull in **different directions on different workloads**,
and the interpreter column is what attributes each move:

- **Tier effect (default vs interpreter column moves).** The baseline JIT is a clear win for
  **`foreign`** (0.66×–0.92×; its interpreter column is *worse* than `BEFORE`) and for
  **`gc_bound churn`** (0.89× vs the interpreter column's 1.82× — churn's module was already
  switch-safe under the old predicate, so `BEFORE` is a JIT figure, and the interpreter is simply
  slower). It is neutral for `baseline`/most `slice` cases, `list_walk` (the JIT does not inline that
  recursion) and `copy_bound`'s `etf_roundtrip`/`ets_copy` — and it is a **regression** for
  `slice`'s `string_str_length` (12.3×) and `queue_from_size` (1.64×), i.e. short-lived processes
  whose first entry pulls in a large Erlang module graph (see *Caveats*).
- **Dispatch-prologue effect (interpreter column moves against `BEFORE`).** `list_walk` is the
  clean signal: a BIF-free *source* loop, but every walked element's `H + sum(T)` lowers to a `gc_bif`
  and the interpreter's `gc_bif` arm is the *same* `call_bif` hot path as `bif`/`call_ext*`
  (`beam_engine/src/interpreter/mod.rs:210`), so it is dispatch-bound — and it improved **1.85×** in the
  **interpreter** column (312.97 → 169.23 ns/element), i.e. with the tier held out. `copy_bound`'s
  `send_recv` also improved in the interpreter column (0.89×). The `copy_bound`
  `etf_roundtrip`/`ets_copy` rows read ~1.1× *worse* in **both** columns — those are inside the
  ~10–15% per-pass spread of this loaded host, so they are not attributed to either change.

## Per-call dispatch — `perf_bif` (3 runs, min of 3)

Same harness, same default slope points (`N1 = 16 000 000`, `N2 = 4·N1`, fastest of 5 after one
discarded warm-up per point) as `BEFORE`:

| workload | ns/call BEFORE | ns/call AFTER (min of 3) | observations (3 runs) |
|---|---|---|---|
| `pc:add` (native-inlined `gc_bif` loop) | 4.03 | **4.39** | 4.97 / 4.39 / 4.63 |
| `pc:loop` (native-inlined `gc_bif` loop) | 4.47 | **5.34** | 5.49 / 5.44 / 5.34 |
| marginal `pc:loop − pc:add` | 0.08 | **0.53** | 0.53 / 1.05 / 0.71 |

The **native-loop** cost is ~9% higher than `BEFORE` (+0.36 ns) and the marginal-op figure is noisier
(0.53 vs 0.08 ns — both are *below one cycle* and the harness itself documents this difference as
unresolvable at this resolution). As `BEFORE` already states, this fixture does **not** exercise the
registered-BIF path — `N-1`/`N+1` are inlined `gc_bif` instructions.

### Registered-BIF (non-inlinable) per-call — the `BEFORE` discrepancy, resolved

The same two-point slope harness `BEFORE` describes, on the same fixture shapes (`nb:bif/2` =
`bif(N-1, Acc+trunc(N))`, `nb2:absb/2` = `absb(N-1, Acc+abs(N))`, the inlined-arithmetic
`nb:arith/2` = `arith(N-1, Acc+N)` as the baseline), driven through the runtime's own `run` action
with the points **interleaved** to cancel load drift, best-of-5 at `N1 = 4M` (bracket best-of-3),
fixtures compiled with the system `erlc` into `$TMPDIR` (**never in the repo**):

| workload | ns/call BEFORE | **ns/call AFTER (default, interleaved best-of-5)** | bracket (best-of-3) | ns/call AFTER (`BEAM_RT_TIER=interpreter`) |
|---|---|---|---|---|
| `nb:arith/2` (inlined `gc_bif` arithmetic baseline) | 5.27 | **6.17** | 5.02 | 750.85 |
| `nb:bif/2` — `erlang:trunc/1` (`call_ext`) | 302.1 | **85.52** | 89.34 | 715.92 |
| `nb2:absb/2` — `erlang:abs/1` (`call_ext`) | 322.3 | **96.13** | 96.53 | 718.03 |
| **marginal registered-BIF round trip** (`bif` − `arith`) | ≈ 297 | **≈ 79** | ≈ 84 | *(not separable — both are interpreter-bound)* |

Both `BEFORE` and `AFTER` figures here are taken on the **baseline JIT** (the `nb` module's only
`call_ext` targets are registered BIFs, which the old switch-safety predicate proved synchronous, so
`BEFORE` ran it on the JIT too — its `nb:arith` 5.27 ns matching `BEFORE`'s JIT-inlined `pc:add`
4.03 ns is the confirmation). The comparison is therefore like-for-like, and it **resolves the
discrepancy `BEFORE` flagged**: the ~300–320 ns/call it measured was a genuine per-call cost at that
HEAD, not a harness artefact. The dispatch-prologue cut (`c9d2b9d7fd`) brings the non-inlinable
registered-BIF round trip to **~79–96 ns/call**, a **≈ 3.5×** reduction, which also matches the
`TRACE_AND_PERF.md` narrative's ~116 ns/call order of magnitude and the cross-crate note of
"~283 ns → ~76 ns/iter". The interpreter column is reported only to show the tier is real: with the
interpreter pinned, *every* loop (inlined or not) is ~715–751 ns/call, because the interpreter has no
native `gc_bif` fast path.

## Caveats that invalidate a comparison

- **A one-shot, module-graph-heavy process paid the whole compile cost at this HEAD** — the
  `string_str_length` 11.27×, `queue_from_size` 1.61× and `send_recv` 1.48× regressions the `AFTER`
  table records were the **eager whole-module compile** this HEAD carried; the `CURRENT` snapshot below
  measures the post-lazy-compile behaviour of the same shape (≈ 1.2×, see its cold-start section). What
  remains is a per-`beam_rt`-OS-process cold start amortised across a long-running node (one
  thread-local compiled-function cache), so the *interpreter* column is the controlled comparison for a
  one-shot case.
- **Absolute latencies are load-polluted** (see the host-load caveat); only ratios/minima are signal.
  `sys` startup `min` moved from 136.52 ms (`BEFORE`) to 160.47 ms (`AFTER`) purely from host
  contention — the `sys` runtime did not change between the snapshots.
- **The `AFTER` figures are min-of-3 within their own mode**, exactly as `BEFORE`'s are; the `sys` and
  `rust` columns of a ratio can therefore come from *different* passes (that is also true of `BEFORE`).
- `BEFORE`'s `foreign`/`list_walk`/`copy_bound`/`gc_bound` rows were **not** all produced on the same
  tier (the old predicate put some modules on the JIT and others on the interpreter), so a reader
  should treat the *interpreter* column — not the raw `BEFORE` → `AFTER` move — as the controlled
  comparison for the dispatch-prologue effect.
- **Nothing was skipped**: all six `Runtimes::Both` workloads ran in all six passes (`status=ok`) on
  both runtimes. The `SysOnly` external targets (`cowboy`, `khepri`, `plb`, `emqx`, `erlc_compiler`,
  `elixir_compiler`) remain out of scope for this Rust-side snapshot exactly as in `BEFORE`
  (`emqx` still `skipped(emqx-requires-otp-28)`).

# `CURRENT` snapshot — HEAD `da82ffe33c`

## Method

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

### Commands

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
was written under `$TMPDIR/perf-now/` and is **ephemeral** (not committed — this task adds only this
file); re-run the commands above to regenerate.

### Host-load caveat (read before quoting any absolute number)

The host is the same shared/oversubscribed one `BEFORE`/`AFTER` describe, but this pass ran in a
**light window**: the 1-min `/proc/loadavg` stayed at **≈ 0.8–14** (mostly below 8) against
`BEFORE`/`AFTER`'s ≈ 20–55. **Absolute latencies here are therefore lower than the two earlier
snapshots for that reason alone** — `sys` startup `min` reads **98.64 ms** against `AFTER`'s 160.47 ms,
although the `sys` runtime did not change between the snapshots. **Absolute latencies are not
comparable across the three snapshots**; only the **ratios** and the **minima** are signal, and the
driver is runtime-major (all `sys` rows, then all `rust` rows) within one report, so the mitigation is
again the three **block-interleaved** passes (`sys,rust | sys,rust | sys,rust`). Read the `CURRENT`
figures as a **loaded-host measurement in a light window**, not a clean-machine number.

## Current figures (`rust` / `sys`, min of 3, `--full` focused cross-runtime pass)

Same reading convention as `BEFORE`/`AFTER` (**< 1 ⇒ Rust cheaper, > 1 ⇒ Rust dearer**; the ratio is
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

## `rust` default vs `BEAM_RT_TIER=interpreter` — the tier effect (min of 3)

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

## The one-shot cold-start cost — current characterisation

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

### The interpreter path's per-instruction reduction charge — controlled A/B

The `INTERP` column above and `## The one-shot cold-start cost` both compare **tiers**; neither
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

## Caveats that invalidate a comparison (`CURRENT`)

- **Absolute latencies are load-polluted**, and this pass ran in a **light window** (≈ 0.8–14 versus
  `BEFORE`/`AFTER`'s ≈ 20–55); only ratios/minima are signal. Do **not** compare absolute ms/ns across
  the three snapshots (`sys` startup `min` 98.64 ms here vs 160.47 ms `AFTER` is host load alone).
- **The `CURRENT` figures are min-of-3 within their own mode**, exactly as `BEFORE`/`AFTER`'s are; the
  `sys` and `rust` columns of a ratio can therefore come from *different* passes.
- **Nothing was skipped**: all six `Runtimes::Both` workloads ran in all six passes (`status=ok`) on
  both runtimes. The `SysOnly` external targets (`cowboy`, `khepri`, `plb`, `emqx`, `erlc_compiler`,
  `elixir_compiler`) were **not run** in this pass — sys-only, so there is no Rust-side figure to
  snapshot (`BEFORE`/`AFTER` record `emqx` as `skipped(emqx-requires-otp-28)`).

## See Also

- [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md) → the bench driver's authoritative
  environment record, per-target tables, methodology and honesty audit (this file links to it and does
  not reproduce it). Its `m6` bands are a **different, far more heavily loaded** pass and are not the
  `CURRENT` baseline.
- [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) → the `perf_bif` harness, the dispatch-cost narrative and the
  execution-trace seam.
- [`CONTEXT.md`](CONTEXT.md) → the runtime crate's contracts and current state.

# `DISPATCH-CUT` snapshot — HEAD `af6d8b07bd` (+ `1b643ead0b`)
## What changed
Three cuts to the **registered-BIF dispatch prologue** (`src/context/calls.rs`,
`src/context/dispatch.rs`, `src/nif.rs`):
1. **The memoized `BifFn` is invoked directly.** `call_registered` now takes the resolved
   `BifFn` (from `DispatchMemo`, via `call_bif_routed`'s `dispatch.bif`) and calls it through the
   new `invoke_registered`, instead of re-running `BifRegistry::call` → `BifRegistry::lookup` —
   i.e. the second `HashMap<String,_>` SipHash lookup per call is gone. `BifRegistry::lookup`
   remains the *only* source of truth for "is this key registered": the memo is never consulted
   as a registry, so it cannot drift.
2. **`dispatch_slot` hashes the key's bytes** (FNV-1a over `module`, `0xFF`, `function`, then the
   arity), not just the name *lengths*. The old length-only mix made same-length keys collide by
   construction (`ets:insert/2` and `ets:lookup/2` shared a slot), so an ETS insert/lookup loop
   was a permanent memo **miss**, paying the whole miss prologue on every iteration.
3. **`NifRegistry::is_empty()` short-circuit** in `try_call_nif`, so an empty NIF registry costs
   one `RefCell` borrow + one length compare instead of a third SipHash lookup.
Items 4 (`RuntimePorts::new` construction) and 7 (`BifContext` size) from the ranked list were
**not** taken: item 4's unconditional construction is the only thing that keeps the shared
`ports` cell reachable from every arm's `BifContext` (gating it needs nine more field types for
~two `Rc` bumps), and item 7's 20 fields are built through one chained builder shared by all
arms. `reentrancy_capable` (the arity-proportional `is_fun` scan) was **kept** — it is what makes
the shared-tables borrow safe. The rewind preconditions (`safe_to_rewind`: empty source table,
`!reentrant`, `Ok`, scheduled process, successful copy) and the `pending.is_empty()` guard before
`atoms_mut()` are unchanged.
## Method
`examples/perf_call.rs` (in-process, best of 5, 100 000 calls/sample, one warm-up run), **release**
profile. Marginal = `ns/call − control`; the control is the same loop with inlined arithmetic only.
**BEFORE** = the pre-cut tree, **AFTER** = the cut tree (both rebuilt at their own HEAD). Three
runs each; the tables below are the per-run figures, not an average.
```sh
# BEFORE (source stashed) and AFTER (source in place), identical commands:
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p beam_rt --example perf_call
erts_rust/target/release/examples/perf_call          # 3 x, /proc/loadavg printed by the harness
```
### Host-load caveat (read before quoting any absolute number)
This is a shared, oversubscribed 14-CPU host and the two windows were **not** equally loaded:
BEFORE ran at 1-min loadavg **22.13 / 22.13 / 20.60**, AFTER at **25.72 / 25.72 / 23.82**, the
AFTER-FIXED pass at **2.51 / 2.31 / 2.31**. Absolute ns therefore do **not** compare across the
three blocks; the **marginal-vs-control** figure and the **before/after ratio at the same row**
(with both sides named) are the signal, and even those carry the load caveat.
## Registered-BIF per-call cost — `perf_call`
Path A = `RuntimeContext::new` (the `-eval` shape: copy-out skipped by construction).
Path B = `RuntimeContext::for_process` (the scheduler shape: copy-out + pooled rewind ON).
| row (path A) | BEFORE ns/call | AFTER ns/call | ratio (AFTER/BEFORE) |
|---|---|---|---|
| `erlang:abs/1` | 67.37 | 46.51 | 0.69 |
| `erlang:trunc/1` | 71.28 | 53.81 | 0.75 |
| `lists:reverse/1` | 89.13 | 65.42 | 0.73 |
| `erlang:'++'/2` | 74.78 | 53.94 | 0.72 |
| `lists:seq/2` | 99.99 | 72.29 | 0.72 |
| **control** (no BIF call) | 0.11 | 0.11 | 1.00 |
| `ets:insert/2` (A2) | 214.34 | 196.26 | 0.92 |
| `ets:lookup/2` (A2) | 165.96 | 149.70 | 0.90 |
| `erlang:abs/1` (B) | 70.79 | 49.06 | 0.69 |
| `lists:reverse/1` (B) | 144.04 | 126.76 | 0.88 |
| `lists:seq/2` (B) | 215.66 | 192.87 | 0.89 |
| `ets:insert/2` (B2) | 222.20 | 199.19 | 0.90 |
| `ets:lookup/2` (B2) | 234.90 | 203.19 | 0.86 |
The marginals (ns/call − control) move by the same ratios, since the control is ~0.1 ns/call on
both sides. Reading: the **immediate-returning** rows (items 1 + 3) fall **25–31 %**, the ETS rows
(the two names that shared a slot) fall **8–16 %**, and the container rows of path B fall **11–12 %**
— the copy-out + rewind they pay dominates them, so the prologue cut is a smaller share there.
## The path-C regression the byte hash introduced — found and fixed
The byte hash put `lists:reverse/1` **and** `erlang:length/1` in the same memo slot, so every path-C
(`Runtime::run_scheduled`, a compiled `lists:reverse/1` + `erlang:length/1` loop) call was a permanent
**miss** and paid the whole prologue (registry lookup + ten `needs_*` `binary_search`es + two `String`
allocations) per iteration — path C at **659.53 / 663.73 / 645.48** ns/iteration, **2.3× dearer** than
BEFORE's 285.23 / 284.96 / 286.29 and reproducible at *low* load too (662.31 at loadavg 3.97).
**Fix:** a fixed avalanche multiply after the FNV fold, taking the high bits
(`dispatch.rs::dispatch_slot`), chosen so the measured hot key set (`lists:reverse/1`, `erlang:length/1`,
`ets:insert/2`, `ets:lookup/2`, `erlang:abs/1`, `erlang:trunc/1`, `erlang:'++'/2`, `lists:seq/2`) maps to
**eight distinct slots**; a collision costs a recomputed prologue and never a different answer (the memo
tags every entry with its key). The memo is `DISPATCH_MEMO_SLOTS` = 8 slots, pinned by
`src/context/tests.rs` (the 16-key probe set exceeds the memo so slot churn is exercised).
### AFTER-FIXED pass (3 runs, loadavg 2.51 / 2.31 / 2.31)
| row | run 1 | run 2 | run 3 |
|---|---|---|---|
| `erlang:abs/1` (A) | 64.34 | 68.46 | 65.02 |
| `erlang:trunc/1` (A) | 58.23 | 50.60 | 58.88 |
| `lists:reverse/1` (A) | 64.96 | 64.02 | 64.13 |
| `erlang:'++'/2` (A) | 51.40 | 52.09 | 52.20 |
| `lists:seq/2` (A) | 74.11 | 72.87 | 72.40 |
| control (A) | 0.15 | 0.17 | 0.16 |
| `ets:insert/2` (A2) | 177.88 | 185.40 | 174.90 |
| `ets:lookup/2` (A2) | 139.08 | 144.36 | 140.67 |
| `erlang:abs/1` (B) | 45.42 | 46.07 | 47.13 |
| `lists:reverse/1` (B) | 118.31 | 120.52 | 116.75 |
| `lists:seq/2` (B) | 189.14 | 192.90 | 184.80 |
| `ets:insert/2` (B2) | 176.57 | 195.08 | 175.77 |
| `ets:lookup/2` (B2) | 179.86 | 198.55 | 195.20 |
| **block C slope** | **223.86** | **223.41** | **213.90** |
Path C is back **below** the BEFORE band (≈ 0.78× of 285.23 at a *lower* load — do not read the
0.78 as a clean speed-up; the load differs). The in-process rows sit within noise of the buggy-AFTER
rows (their keys were never colliding in the offending slot), so the fix is specific to the
collision, not a general re-measurement.
## The `emqx_base62:encode/1` A/B — controlled, copy-out ON vs OFF
Between m9 and m10 `tools/bench/RESULTS.md` recorded `emqx_base62_encode` at 10 283 → 16 601 ns/call
(+61.4 %) while its three sibling `foreign` cases improved 8–9 %; two hypotheses were on the table —
**(i)** the m10 copy-out seam (`move_result_into_process_heap` + its rewind guard) made
container-returning calls dearer, **(ii)** m9's figure was anomalously low.
**A/B (one binary, copy-out toggled by a temporary never-committed edit, reverted completely before the
final tree):** `ab_drv:enc/1` (the `encode/2` shape) reads **1 375 853 ns/iteration** with copy-out **ON
and OFF** — identical to three significant figures — and `ab_drv:dec/1` 37 341.60 ns/iteration.
**Verdict: neither hypothesis is supported** — the copy-out contributes **≈ 0 %**.
The blow-up is in `emqx_base62.erl` itself: `encode/2` appends
`<<Acc/binary, (iolist_to_binary(CharList))/binary>>` once per group of three input bytes, a
**superlinear** linear-append idiom under an interpreter, each `iolist_to_binary/1` a real registered BIF
call (copy-out ON, loadavg 2.32: `sum/1` 25 000 → 20 ms, 100 000 → 26 ms; `enc/1` 1930 ms → 101 522 ms).
This tree's own `foreign` `emqx_base62_encode` row reports **76 698 ns/call** (vs 16 601 in m10) —
consistent with a superlinear workload measured at different operating points.
**The A/B resolves** the copy-out's direction and order of magnitude (≈ 0 %) and rules out the m10 seam as
the mover, but **cannot** produce a per-call figure (the workload has no stable per-call cost at any N).
It also produced a negative result: with copy-out forced off, `perf_call`'s path-B loop **dies with exit 1**
(`perf_call: lists:reverse/1 raised system_limit`) — the pool exhaustion the copy-out + rewind prevents.
## Gate
`erts_rust/tools/differential/gate.sh` on this tree: `differential gate: total=2497 agree=2495
mismatch=0 unsupported=2 skipped=0 allowed=2 unauthorized=0 stale=0` → **PASS (exit 0)** at that HEAD;
the recorded capability gap is `wire/new_fun_ext_encode` (A.5). The A.9
`boot_extra/init_script_id_names_the_running_otp_release` case **agrees** on the current tree: with no
`-boot` the default route drives this runtime's own release's script
(`CodeServer::own_bootstrap_boot_script`), so `init:script_id()` answers `{"Erlang/OTP","30"}`,
matching `erlang:system_info(otp_release)` (`"30"`). A dispatch change is
behaviour-adjacent, so this is the authoritative check that no result moved.
## Caveats that invalidate a comparison (`DISPATCH-CUT`)
- The three windows have different host load (22.13 / 25.72 / 2.51 1-min); only per-row ratios and
  marginals are signal, and never across windows without naming both sides.
- Path C is a *mixed* row: each iteration is one `lists:reverse/1` `call_ext` **plus** one
  `erlang:length/1` `call_ext` plus the engine's surrounding instructions, so it prices the memo
  (hits vs misses) rather than any single BIF.
- The `MIX` constant in `dispatch_slot` is **chosen against the measured hot key set**, not derived;
  a workload whose hot key set differs can still collide (cost only, never a wrong answer). The
  8-slot table is the binding limit; enlarging it requires the `src/context/tests.rs` churn
  assertion to change with it.
- **Cold start:** `perf_call`'s path C spawns/loads per run and the example compiles the fixture
  with `erlc` once per invocation, so its *first* samples carry first-entry compile + load cost;
  the slope cancels that, the absolute ms do not.
## The BIF-result copy-out cut (`COPY-OUT-CUT`)

HEAD `9d238e22bc8f143cec056e95ca9d19af0fa960b7` (BEFORE) → `4de5b69109e93a116c38793054b472fab35190ec` (AFTER).
Host: 8-core x86-64 dev container, loadavg 1-min **6.55 → 20.5** during the BEFORE window and
**≈ 1.15** during the AFTER A/B (per-block `/proc/loadavg` is quoted with each table below, exactly as
`perf_call` prints it). Release profile. Commands:

```
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_call -- 100000   # BEFORE (HEAD 9d238e22bc)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_call -- 100000   # AFTER  (HEAD 4de5b69109)
```
The harness measures best-of-5 per row at 100 000 calls/sample and prints `/proc/loadavg` before and
after every block; its own `control` row (the same loop with inlined arithmetic only) is the floor
that every marginal is taken against.

### What changed
`src/context/calls.rs::move_result_into_process_heap` used to pay, on every container-returning BIF
call in the scheduler shape (path B): a **failed** copier preflight walk, then
`beam_term::size_object(term)` (a *second* full walk, plus a `HashSet` and a `Vec`), then a fresh
`OffHeapTable` + `CopyContext`, then a second copy — up to three graph traversals where one copy needs
one. `copy_struct`'s refusal is a preflight: it refuses **before** writing any destination word, so the
first attempt is an entire wasted walk. The new body sources the growth hint from the refusal the
copier itself already produced (`AllocFailure::requested / size_of::<Eterm>()`, the `beam_message::error`
contract's exact flat word count — no second traversal), gives each attempt its own **fresh**
`OffHeapTable` destination (which is what keeps the copier's fast path eligible), and gates the retry on
`beam_message::is_message_heap_exhaustion` so a refusal
growing the destination cannot fix (above all the malformed-graph `badarg`) no longer grows a whole
space and fails identically. Nothing in the frozen precondition set was weakened: the rewind still
requires an empty source `OffHeapTable`, `!reentrant`, an `Ok` result, a scheduled process
(`mailbox_table.is_some()`) and a **successful** copy before `Heap::reset_to(mark)`; a term that cannot
be re-homed leaves both the result and the pool exactly as they were.

### BEFORE — path A (`RuntimeContext::new`, copy-out skipped by construction), 100 000 calls
loadavg 1-min: 6.55 / 2.00 / 4.00 across the three runs.

| row | run 1 | run 2 | run 3 |
|---|---|---|---|
| `erlang:abs/1` | 64.86 | 69.11 | 64.05 |
| `erlang:trunc/1` | 67.08 | 80.13 | 72.47 |
| `lists:reverse/1` | 65.15 | 95.82 | 65.00 |
| `erlang:'++'/2` | 53.50 | 78.56 | 52.35 |
| `lists:seq/2` | 68.80 | 72.67 | 70.28 |
| `ets:insert/2` (A2) | 175.53 | 177.09 | 176.79 |
| `ets:lookup/2` (A2) | 144.47 | 139.29 | 142.76 |
| **control** | 0.16 | 0.16 | 0.15 |

### BEFORE — path B (`RuntimeContext::for_process`, copy-out + pooled rewind ON)
Same three blocks/loads as above.

| row | run 1 | run 2 | run 3 |
|---|---|---|---|
| `erlang:abs/1` | 46.98 | 47.00 | 46.02 |
| `lists:reverse/1` | 115.57 | 115.76 | 114.98 |
| `lists:seq/2` | 187.28 | 188.11 | 184.06 |
| `ets:insert/2` (B2) | 182.29 | 181.71 | 177.38 |
| `ets:lookup/2` (B2) | 183.03 | 176.90 | 177.47 |
| **block C slope** | 224.28 | 221.69 | — |

Copy-out delta B−A (marginal, the harness's own `copy-out delta` table): `lists:reverse/1` **+50.42 /
+19.94 ns**, `lists:seq/2` **+118.48 / +115.45 ns**, `ets:lookup/2` **+38.56 / +37.61 ns** — taken
against a 0.16 ns control.

### AFTER — interleaved binary A/B (`BASE` = HEAD `9d238e22bc`, `OPT1` = the new body)
Same script, same host, same 30-minute window, loadavg **≈ 1.15** throughout (1-min);
`BASE`/`OPT1`/`OPT1`/`BASE` per round, two samples each, path B only (the block the change touches).
This interleaved binary A/B — not a sequential BEFOR/AFTER pair — is the AFTER evidence: the shared
host's load varied by more than the effect across the session, and interleaving is what makes the
ratio citable.

| row (path B) | BASE min / mean | OPT1 min / mean | ratio (OPT1 / BASE, mean) | ratio (OPT1 / BASE, min) |
|---|---|---|---|---|
| `lists:reverse/1` | 115.20 / 117.95 | 110.14 / 112.02 | **0.950** | 0.956 |
| `lists:seq/2` | 185.84 / 190.09 | 164.74 / 166.65 | **0.877** | 0.886 |
| `ets:lookup/2` (B2) | 177.94 / 185.61 | 175.65 / 180.94 | **0.975** (noise-dominated) | 0.987 |
| control | 0.15 | 0.15 | 1.0 | 1.0 |

Every `OPT1` sample was ≤ its paired `BASE` sample within every round. **Marginals vs the control:**
`lists:reverse/1` **≈ −5.9 ns/call** (117.95 → 112.02), `lists:seq/2` **≈ −23.4 ns/call**
(190.09 → 166.65), `ets:lookup/2` **≈ −4.7 ns/call** (inside the row's own spread — not claimed as a
win). The control is unchanged at 0.15 ns, so the deltas are the copy-out's, not the harness loop's.

### Verdict
**The cut wins where the copy-out dominates, and it does not regress anything measured.**
`lists:seq/2` — the row with the largest graph per call — improves **≈ 12 %** (`lists:seq/2` OPT1/BASE
mm 0.877), `lists:reverse/1` **≈ 5 %** (0.950), `ets:lookup/2` is unchanged within noise (0.975/0.987).
`erlang:abs/1` is an immediate result and never enters the copy-out at all, so it is expected flat
before *and* after. **Unchanged rows are reported as unchanged:** the ETS path B2 row is not claimed.
Absolute latency is not signal on this host — the BEFORE window ran at load 6.55–20.5 and the AFTER
window at ≈ 1.15, so no BEFORE ns/call is compared to an AFTER ns/call anywhere above; only the
interleaved ratios and the marginals-vs-control are.

### Negative results (measured, then discarded)
- **"Size first, grow if needed, one copy"** (temporary, never-committed edit): `size_object(term)`,
  `if available < needed { grow_nursery(needed.max(256)) }`, then one `copy_struct`. Measured **≈ 290
  ns/call for `lists:reverse/1`** against a BASE of ≈ 121 ns — **≈ 2.5× worse** — *and* it grew the
  nursery on **every** call (the target `max(capacity, next_words(need, 0))` walked 3→6→12→…→256
  words), i.e. it strictly worsens the item-4 peak-RSS effect below. Discarded on both counts.
- **Why a size-first policy loses in general:** `beam_term::size_object` and the copier's own
  `preflight_words` cost the same order, but a size-first policy pays that walk on **every** call
  whereas the retry policy pays the copier's own bounded walk only on the (in the scheduler shape,
  every-call) refusal path — and the refusal already carries the count back.
- **Forcing the copy-out off entirely** (temporary edit): `perf_call` path B dies with
  `beam_rt: lists:reverse/1 raised system_limit`. The copy-out + `reset_to(mark)` rewind is what
  bounds the 16 MiB pooled BIF heap; it is load-bearing and was never removed.
- **Instrumentation count:** a temporary `eprintln!` in the failure arm showed **32 001 failure
  events for a 20 000-iteration run** — the retry really does fire on essentially every container
  copy-out in the scheduler shape, which is what makes the removed `size_object` walk a per-call cost.

### Reachability of the second attempt
The retry is now reached **only** for `beam_message::is_message_heap_exhaustion` — i.e. the copier's
typed `AllocKind::Message` refusal from its own preflight. Every other refusal a legal BIF result can
produce (`BadArg` for a malformed graph, an `AllocKind::Heap`/`LiveObject` failure from a nested push,
a `Raised` reason) is permanent for this destination and now returns `None` immediately. Pinned by
`src/context/calls.rs::tests::a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing` (a
`make_header(0, 0)` term: the nursery's `young_space_count()` and the destination's `used()` are both
unchanged and the call answers `None`) and its positive control
`an_exhaustion_refusal_grows_the_nursery_and_the_retry_copies` (an oversized `lists:seq(1, 2000)`
result still grows the nursery and is copied on the retry).

### The copy-out destination is always fast-path-eligible
`beam_message`'s copier takes its allocation-free single-traversal path (`try_copy_fast`) exactly when
`copy.rs::is_fast_path_eligible` holds, i.e. when the destination's `OffHeapTable` is fully empty in all
three of its lists — the destination **heap**'s `used` is deliberately not part of that gate.
`src/context/calls.rs::copy_result_fresh` builds a **fresh** `beam_message::OffHeapTable::new()` for each
copy attempt, so the gate is true every time and every copy-out is served by the one traversal, with no
`preflight_words` walk before the first word is written.
The `grow_nursery` retry gets its **own** fresh table, so a partial attempt's registrations can never
reach it.
On success the local table's off-heap references are adopted into `Runtime::eval_off_heap`; on a refusal
nothing is adopted and the dropped table releases the node references the abandoned attempt took.

### Gate
`erts_rust/tools/differential/gate.sh` on the AFTER tree: `differential gate: total=2497 agree=2495
mismatch=0 unsupported=2 skipped=0 allowed=2 unauthorized=0 stale=0` → **PASS (exit 0)**, against a
real oracle (`erl` present on `$PATH`). The recorded capability gap is `wire/new_fun_ext_encode` (A.5);
the A.9 `boot_extra/init_script_id_names_the_running_otp_release` case **agrees** on the current tree:
with no `-boot` the default route drives this runtime's own release's script, so `init:script_id()`
answers `{"Erlang/OTP","30"}`, matching `erlang:system_info(otp_release)` (`"30"`).
A copy-out change moves terms between heaps, so this is the authoritative check that no result moved.

### Caveats that invalidate a comparison (`COPY-OUT-CUT`)
- The BEFORE and AFTER windows have very different host load (6.55–20.5 vs ≈ 1.15 1-min). **No BEFORE
  ns/call is compared to an AFTER ns/call anywhere in this section**; the AFTER evidence is the
  interleaved `BASE`-vs-`OPT1` binary A/B, which shares one window and one load.
- `erlang:abs/1` is under 50 ns and the `control` under 0.2 ns, so both are at the resolution floor;
  neither is used to claim a win.
- **A fresh 3-run AFTER pass on the final committed HEAD was not run** — the turn budget went to the
  code, the units and the gate. The A/B above is on the same body that the commit `4de5b69109` pins;
  re-run the two commands at the top of this section to reproduce a sequential pair.
- The change does **not** address the item-4 growth in retained nursery space (see below): it removes a
  wasted graph walk and a needless retry, not the `grow_nursery` itself.

### `ets_copy` peak RSS (measured 94.68 MiB at N = 100 000, still linear in N) — a structural limit, not the ETS adopt path
**`ets_copy` peak RSS (measured 94.68 MiB at N = 100 000, still linear in N) — a structural limit, not the ETS adopt path.** The recorded `RESULTS.md` §8 item-5 lead (the ETS adopt path / the rewind guard) is **refuted**: `safe_to_rewind` is already true for `ets:*` and the pooled heap is already rewound. The growth is per-iteration, ≈790 B ≈ 99 words, and it was bisected: a float-free ETS loop of the same shape grows identically, while a float-carrying churn loop with no ETS is flat — so it tracks **BIF calls**, not floats and not ETS table state. Mechanism: almost all of the loop's heap traffic is the BIF-result copy-out, which by design cannot collect (the machine's live x registers / Erlang stack are not published at a BIF call, so a collection there would free live terms); a full nursery is met by `Process::grow_nursery(hint)` (`src/context/calls.rs::move_result_into_process_heap`), and a **retired young space is retained**, so the retained capacity accumulates linearly. The loop never reaches a real collection either: `ensure_heap_full` returns at `!process.should_collect(heap_need)` (`src/context/gc.rs` → `beam_gc`'s `nursery.current().available_words() < need_words`) because the loop's own `test_heap` prefixes are tiny (a 233-word default nursery, `beam_gc/src/policy.rs`), and `alloc_words_rooted` is never entered (the BIF, not the engine, builds the result). Removing the caller-module float guard at the two FR-publishing GC doors was tried and **reverted**: it changes nothing here (94.68 → 95.02 MiB) and contradicts the pinned float-collection contract. Closing this needs either a **`beam_process`** seam that can shrink/reuse a retired young space (`Process::grow_nursery` retains it) or a **`beam_engine`/runner** seam that publishes the driving machine's roots at the BIF call so the copy-out can collect — both sibling-crate changes, not reachable from `src/context/calls.rs`.
