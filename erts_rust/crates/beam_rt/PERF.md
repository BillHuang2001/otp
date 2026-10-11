# `beam_rt` — performance snapshot (`BEFORE`)

A **current-state** measurement snapshot of the Rust runtime, taken at the commit below so a later
change to the execution engine / registered-BIF dispatch path can be compared against it.
It is **not** a change log and it does **not** supersede the tool-specific records — for the bench
driver's full environment record, its per-target tables and its methodology see
[`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md); for the per-call harness and the
dispatch-cost narrative see [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md).

Every number here is **measured, never modelled**. Nothing was fabricated; the sys-only workloads
that cannot produce a Rust-side figure are recorded as `skipped(<reason>)` with no number.

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

## See Also

- [`../../tools/bench/RESULTS.md`](../../tools/bench/RESULTS.md) → the bench driver's authoritative
  environment record, per-target tables, methodology and honesty audit (this file links to it and does
  not reproduce it).
- [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) → the `perf_bif` harness, the dispatch-cost narrative and the
  execution-trace seam.
- [`CONTEXT.md`](CONTEXT.md) → the runtime crate's contracts and current state.
