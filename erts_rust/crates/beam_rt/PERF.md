# `beam_rt` — performance snapshot (`BEFORE` → `AFTER`)

Two **current-state** measurement snapshots of the Rust runtime, taken at two commits with the
**same method** so the two behavioural changes that landed between them — the execution-tier default
and the registered-BIF dispatch prologue — can be read off directly. The `BEFORE` snapshot below is
the original record and is kept **verbatim**; the `AFTER` snapshot is measured on the current tree.
It is **not** a change log and it does **not** supersede the tool-specific records — for the bench
driver's full environment record, its per-target tables and its methodology see
[`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md); for the per-call harness and the
dispatch-cost narrative see [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md).

Every number here is **measured, never modelled**. Nothing was fabricated; the sys-only workloads
that cannot produce a Rust-side figure are recorded as `skipped(<reason>)` with no number.

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
whole-process cost is far lower under Rust (`baseline` startup **0.126×**, `slice` cases
**0.063×–0.238×**), while the per-unit interpreter cost is far higher (**9.6×–120.5×** across the
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
non-inlinable BIF (e.g. `trunc/1`) via this same harness. At this HEAD, on this host, the non-inlinable
BIF path reads **≈ 3× higher (~300–320 ns/call)**; the two non-inlinable BIFs agree with each other, so
it is a genuine per-call cost at this state, not a one-BIF artefact. Whether this is a since-reintroduced
dispatch cost or a measurement-context difference is **not reconciled here** and is flagged for the
workstream that owns the dispatch prologue (it is the natural `AFTER` target of this snapshot).

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

- **The new always-JIT default regresses short-lived processes that enter a large Erlang module
  graph.** `slice/string_str_length` (`bslice_string:str_length/1` → `string:length/1`) went from
  **32.85 ms to 405.29 ms** (12.3×) under the *default*, while the *interpreter* column reproduces
  `BEFORE` exactly (33.68 ms). Verified independently of `bench`, on the CLI, and it is a **per-process
  JIT warm-up**, not a per-call cost — in one process, `string:length/1` once costs ~644 ms and
  `string:length/1` *twice* costs ~611 ms (same work, one compile). The same shape is visible at
  smaller scale in `slice/queue_from_size` (21.85 → 35.85 ms). Every case is still **correct** (the
  `run` argument/result self-check passes); it is a cold-start cost the tier selection does not yet
  amortise for a one-shot process. This is the single biggest caveat in the `AFTER` table and is
  flagged for the workstream that owns tier selection — `BEFORE` cannot be compared to `AFTER` on
  *whole-process* wall time for a module-graph-heavy case without holding the tier constant.
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

## See Also

- [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md) → the bench driver's authoritative
  environment record, per-target tables, methodology and honesty audit (this file links to it and does
  not reproduce it). Its `m6` bands are a **different, far more heavily loaded** pass and are not the
  `AFTER` baseline.
- [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) → the `perf_bif` harness, the dispatch-cost narrative and the
  execution-trace seam.
- [`CONTEXT.md`](CONTEXT.md) → the runtime crate's contracts and current state.
