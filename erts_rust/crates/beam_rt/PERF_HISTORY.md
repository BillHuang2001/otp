# `beam_rt` — dated performance snapshots (`BEFORE` → `AFTER`)

Two **dated, HEAD-labelled** suite snapshots of the Rust runtime taken with the **same method**, so the behavioural changes that landed between them read off directly — followed by three **single-mechanism controlled A/Bs** taken later, each carrying its own two HEADs.
All of them are **historical records kept verbatim**: each describes its own tree, never today's, and none is the figure to quote for the present tree.
The present-tree figures are in [`PERF.md`](PERF.md) (`CURRENT` plus the standing *Current startup regime* note), the per-call dispatch and copy-out analyses are in [`PERF_DISPATCH.md`](PERF_DISPATCH.md), the measurement narrative is [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md), and the bench driver's own environment record is [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md).
A `copy-out OFF`/`copy-out ON` or `Path A`/`Path B` arm label names that comparison's arm as it was at its own HEAD — [`PERF_DISPATCH.md`](PERF_DISPATCH.md)'s *Measurement provenance* note records what each arm did then, since both arms of the present tree copy a container result and only the destination differs.
`BEFORE` is the pre-change baseline; `AFTER` is the same method re-run after the execution tier defaulted to the native baseline JIT for every process and the registered-BIF dispatch prologue was cut, with a `BEAM_RT_TIER=interpreter` column added so a **tier effect** reads apart from a **dispatch-prologue effect**.
Every number here is **measured, never modelled**; a sys-only workload that cannot produce a Rust-side figure is recorded as `skipped(<reason>)` with no number.

## Host load for the two suite snapshots (read before quoting any absolute number from either of them)

The benchmark host is shared and other agent builds run concurrently, so the 1-min `/proc/loadavg` sat at **≈ 20–55** (≈ 1.5–4× `nproc`) during `BEFORE` and **≈ 31–52** (≈ 2.2–3.7× `nproc`) across the `AFTER` passes — a somewhat lighter band, with the same non-monotonic relationship to the measured figures.
**Absolute latencies are load-polluted and are not comparable across runs or across the two snapshots**; only the **ratios** and the **minima** are signal.
The driver is runtime-major within one report (all `sys` rows, then all `rust` rows), so every pass here was run **block-interleaved** (`sys,rust | sys,rust | sys,rust`), which is the mitigation used throughout; a strictly interleaved per-workload alternation is not expressible through the CLI (the two-file `bench compare` matches rows by `(workload,runtime)` and does not cross the runtimes).
Read every figure below as a **loaded-host measurement**, not a clean-machine number: `BEFORE` is a **loaded-host baseline** and `AFTER` a loaded-host measurement carrying the same caveat.
Each snapshot's own per-pass `/proc/loadavg` readings are in its own `Method` table.

## `BEFORE` snapshot — HEAD `3f24c5ec9d`

### Method

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

#### Commands

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
**ephemeral** (not committed — re-run the commands above to regenerate).

### Current figures (rust / sys, min of 3, `--full` focused cross-runtime pass)

`rust/sys` = Rust cost as a multiple of the system-ERTS cost (**< 1 ⇒ Rust cheaper, > 1 ⇒ Rust dearer**).
The `sys` / `rust` columns are the **per-unit metric** min-of-3 for each side (best case of each runtime),
for context; the ratio is the headline. `slice` is per-case **whole-process** wall (one call per process);
the `foreign`/`list_walk`/`copy_bound`/`gc_bound` rows are **startup-cancelled marginals**
(`(mean wall@N − mean wall@0) / N`), the interpreter-throughput signal.
These `baseline`/`slice` figures are this snapshot's own preloaded-only-slice measurements; every CLI action now pays the boot script (see [`PERF.md`](PERF.md) *Current startup regime*), so they are not the current whole-process wall.

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

### Per-call dispatch — `perf_bif` (3 runs, min of 3)

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

#### Registered-BIF (non-inlinable) per-call — supplementary measurement

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

### External-repo workloads (sys-only) — not part of this Rust-side snapshot

`cowboy`, `khepri`, `plb`, `emqx`, `erlc_compiler` and `elixir_compiler` are `Runtimes::SysOnly`: they
measure the **system ERTS** and cannot produce a Rust-side figure, so they are out of scope for a
beam_rt BEFORE baseline and were not run here.

| workload | status | reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | **verified** by a direct run — the checkout pins OTP 28 while the host `erl` is OTP 29 (`RESULTS.md` §4.3). No number. |
| `cowboy` | not run | sys-only; builds a relx release with a network `git` fetch of cowlib/ranch (~67 s, `RESULTS.md` §4.4) — neither quick nor Rust-relevant |
| `khepri` | not run | sys-only; `rebar3 eunit` ≈ 80–100 s (`RESULTS.md` §4.2) — not quick and Rust-irrelevant |
| `plb`, `erlc_compiler`, `elixir_compiler` | not run | sys-only (no Rust side to snapshot) |

## `AFTER` snapshot — HEAD `e01965432c`

### What changed since `BEFORE`

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

### Method (identical to `BEFORE`)

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

#### Commands

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
**ephemeral** (not committed — re-run the commands above to
regenerate). The registered-BIF fixtures (`nb.beam`/`nb2.beam`) and their two-point slope script were
built under `$TMPDIR` too — **never in the repo**.

### Current figures (`rust` / `sys`, min of 3, `--full` focused cross-runtime pass)

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

### `rust` BEFORE → AFTER delta

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

### Per-call dispatch — `perf_bif` (3 runs, min of 3)

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

#### Registered-BIF (non-inlinable) per-call — the `BEFORE` discrepancy, resolved

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

### Caveats that invalidate a comparison

- **A one-shot, module-graph-heavy process paid the whole compile cost at this HEAD** — the
  `string_str_length` 11.27×, `queue_from_size` 1.61× and `send_recv` 1.48× regressions the `AFTER`
  table records were the **eager whole-module compile** this HEAD carried; the `CURRENT` snapshot in
  [`PERF.md`](PERF.md) measures the post-lazy-compile behaviour of the same shape (≈ 1.2×, see its
  cold-start section). What remains is a per-`beam_rt`-OS-process cold start amortised across a
  long-running node (one thread-local compiled-function cache), so the *interpreter* column is the
  controlled comparison for a one-shot case.
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

## Further dated snapshots — single-mechanism controlled A/Bs

Each A/B below carries **its own two HEADs** and isolates **one mechanism** with nothing else changed, measured after the `AFTER` snapshot above.

### The interpreter path's per-instruction reduction charge — controlled A/B

`PERF.md`'s `INTERP` column and its *The one-shot cold-start cost* section both compare **tiers**; neither
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
  see `PERF.md` *Current startup regime*) and every `slice` row (whole-process wall for one call per OS process,
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
The differential cases `gc/process_info_gc_key_set_is_stable`, `gc/process_info_memory_is_positive_integer` and `system_info/process_info_bulk_returns_list` all report `[AGREE]`; **no corpus case exercises the `binary` item** — [`tools/differential/src/corpus/gc.rs`](../tools/differential/src/corpus/gc.rs) keeps it out behind a stale capability-gap note, so that item's coverage is `tests/process_info_binary.rs`'s own oracle cross-check (`PERF.md` *`process_info(self(), binary)`*), not the corpus.

**Where the memory ceiling is bounded.** The reservoir is not an accumulator: the runner's `Yield` arm prunes it at the slice boundary whenever the process's binary virtual heap is over its budget, releasing every payload its just-published roots no longer reach (`src/context/gc/bin_vheap.rs`), so `erlang:memory(binary)` is flat in the iteration count — the figures, the sawtooth envelope, the disabled-site A/B and the retained-payload half are [`CONTEXT.md`](CONTEXT.md) *Known Issues / Intentional M1 gaps*'s off-heap bullet, measured by `tests/off_heap_reclaim.rs`.
The end-to-end effect on a real workload — `emqx_base62:encode/1` on the unmodified EMQX module, `PRE-FIX` `dc63fd7f49` vs `HEAD` vs the oracle, all three legs from the identical `.beam` — is [`src/runner/MEASURED.md`](src/runner/MEASURED.md) *The off-heap reclamation end to end*; the mirror A/B above is the per-call half of this machinery, never a memory one.
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

**One shape's cadence did move** (wall time unmoved): `gc_bound/churn`'s loop copies out a 257-word `list_to_tuple/1` result, so its growth now happens *before* the copy, and one growth = one retired space = one unit of collection debt — its schedule shifts from 13 971 collections / 2 997 majors to **15 346 / 2 514** at `N` = 2 000 (7.6728 collections per iteration, one major per iteration plus a fixed 514) against a wall change of 1.012×.
That is a table move, not a door regression: `src/runner/tests/boundary_cost.rs` and `src/runner/CONTEXT.md`'s `MEASURED` table are re-derived from it, and the door's own invariants (`checks == yields`, `unpublished == 0`, the `checks/9 … checks/7` ration, one young space, `capacity ≤ 610`) all still hold.

**Correctness of the measured side.** `--lib` **1 520 passed / 0 failed / 3 ignored**; `--lib -- runner` 47 / 0; `--lib -- copy_out` 3 / 0 (including the collecting-default-process case and its no-hint control, which refuses again); `--lib -- context::calls` 14 / 0 (1 ignored); `--test max_heap_enforce` 6 / 0, `--test max_heap_flag` 12 / 0, `--test process_info_items` 2 / 0, `--test engine_off_heap_transport` 4 / 0, `--test port_wake` 1 / 0. `build --release -p beam_rt -p bench --all-targets` is **warning-free**.

**Residual / owned by other nodes.**
The hint is **per `RuntimeContext`**, so the first copy of each context still learns by refusing: **754 refusals per 250 000 ops ≈ 13 ns/op (0.4 %)**. Removing it needs a `Runtime`-owned pid→hint map with cleanup on process death (this crate, next pass).
The `beam_term::size` hashing walk is now **off this shape's critical path** (it was the #1 `perf` symbol at ~8 % while refusals ran at 0.332/op) — what remains of it is that 0.4 %.
The registry's own copy is untouched: every `ets:lookup` result is still copied twice (`beam_bifs::ets::copy_out` onto the pooled BIF heap, then `copy_result_fresh` onto the process heap; `shallow_unshared::<TrialTarget>` was 5.7–8.1 % of the run) — that is a `beam_bifs`/pool-rewind contract change, not expressible here.
A deeper `beam_gc` fix (feed the collapse the post-copy live demand so a no-relocation growth survives a collection) would make the growth permanent instead of per-space, and `+hmax` still does not bound the growth-retired nursery chain.

## See Also

- [`PERF.md`](PERF.md) → the current figures (`CURRENT`), the *Current startup regime* note and the standing boot-path poll-budget note.
- [`PERF_DISPATCH.md`](PERF_DISPATCH.md) → the per-call `DISPATCH-CUT` / `COPY-OUT-CUT` analyses.
- [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md) → the bench driver's environment record, per-target tables and methodology.
- [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) → the harnesses behind every figure here and the dispatch-cost narrative.
