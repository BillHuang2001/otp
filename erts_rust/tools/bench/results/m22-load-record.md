# m22 host/load record — the GC/allocator workstream's "after" record (2026-10-27)

Session: **m22**, 2026-10-27 (18:31:08–18:36:10 UTC), 14 logical cores, **shared host**. `/proc/loadavg` (1/5/15-min)
was read immediately before and after every pass, exactly one benchmark process ran at a time, and every pass was pinned
with `taskset -c 3 setsid -w`. The 1-min figure ranged **6.97 – 21.85** over the twelve matched passes (median 12.59 over the
24 readings): m22 is a **mid-load** session, in the same band as m21 (5.65–19.57) and unlike the quiet m19 (0.54–4.53).
**What this record is for:** it is the *after* measurement for the GC/allocator workstream — the same four m21 workload ids
(`baseline`, `gc_bound`, `gc_class`, `list_walk`) **plus `slice`**, which m21 did not measure and which carries the
footprint/boot story. Its comparison against m21 is `m22-vs-m21.txt`; the per-round derived table is `m22-xrun-ratios.txt`.

Sides: `sys` = the system ERTS on `PATH` — **Erlang/OTP 29.1.2 / erts-17.1.1**
(`/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl`, `version=29`);
`rust` = `erts_rust/target/release/beam_rt` at its default baseline-JIT tier (`version=30`), i.e. the binary this tree's
HEAD builds. **Oracle caveat, carried deliberately:** the tree pins OTP **30.0-rc0 / erts-17.1**, while the sandbox oracle is
OTP **29.1.2 / erts-17.1.1** — and m21 already named that same 29.1.2 oracle (its archived `runtimes[].program` is the same
store path), so m21 <-> m22 is oracle-matched; the 29.1.1 / erts-17.1 oracle belongs to m19 and older records only (the artifact-less m20 named in `BOTTLENECKS.md` §0 has no reports here).

## Command shape (one matched round = two single-side passes, alternating order)
```sh
WL=baseline,gc_bound,gc_class,list_walk,slice
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --verbose --runtime rust \
    --workloads $WL --out-file erts_rust/tools/bench/out/m22-xrun-r<N>-rust.json
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --verbose --runtime sys \
    --workloads $WL --out-file erts_rust/tools/bench/out/m22-xrun-r<N>-sys.json
```
Side order alternated every round (r1 rust->sys, r2 sys->rust, r3 rust->sys, r4 sys->rust, r5 rust->sys, r6 sys->rust).
**Six** matched rounds (not three: six gives each median a stability column behind it) plus one supplementary `--runtime all`
budget pass. Raw boundary log: `/proc/loadavg` triples at every pass boundary, reproduced verbatim below.

| run | artifact(s) | pin | `/proc/loadavg` (1-min) before -> after | 5-min before -> after | wall |
|---|---|---|---|---|---|
| `r1` rust | `m22-xrun-r1-rust.json` | `taskset -c 3` | 7.51 -> 6.97 | 10.19 -> 9.91 | 20.4 s |
| `r1` sys | `m22-xrun-r1-sys.json` | `taskset -c 3` | 6.97 -> 10.86 | 9.91 -> 10.56 | 21.4 s |
| `r2` sys | `m22-xrun-r2-sys.json` | `taskset -c 3` | 10.86 -> 15.88 | 10.56 -> 11.78 | 24.0 s |
| `r2` rust | `m22-xrun-r2-rust.json` | `taskset -c 3` | 15.88 -> 13.16 | 11.78 -> 11.45 | 21.4 s |
| `r3` rust | `m22-xrun-r3-rust.json` | `taskset -c 3` | 13.16 -> 12.26 | 11.45 -> 11.36 | 20.3 s |
| `r3` sys | `m22-xrun-r3-sys.json` | `taskset -c 3` | 12.26 -> 12.66 | 11.36 -> 11.49 | 20.4 s |
| `r4` sys | `m22-xrun-r4-sys.json` | `taskset -c 3` | 12.66 -> 12.59 | 11.49 -> 11.54 | 20.7 s |
| `r4` rust | `m22-xrun-r4-rust.json` | `taskset -c 3` | 12.59 -> 11.25 | 11.54 -> 11.35 | 21.0 s |
| `r5` rust | `m22-xrun-r5-rust.json` | `taskset -c 3` | 11.25 -> 9.11 | 11.35 -> 10.86 | 20.4 s |
| `r5` sys | `m22-xrun-r5-sys.json` | `taskset -c 3` | 9.11 -> 13.34 | 10.86 -> 11.68 | 20.7 s |
| `r6` sys | `m22-xrun-r6-sys.json` | `taskset -c 3` | 13.34 -> 21.29 | 11.68 -> 13.67 | 26.3 s |
| `r6` rust | `m22-xrun-r6-rust.json` | `taskset -c 3` | 21.29 -> 21.85 | 13.67 -> 14.44 | 25.5 s |
| budget pass (`--runtime all`) | `m22-budgetprobe-all.json` + `.txt` | `taskset -c 3` | 21.85 -> 15.14 | 14.44 -> 13.69 | 40.2 s |

All 13 passes exited `rc=0`. The 15-min figure drifted 15.26 -> 15.33 over the session (the machine's background
load, not this measurement), so the 1-min column is the one that matters for drift and it is recorded at both boundaries.

## What the load did to each side (per-round stability evidence)
| round | sys startup p50 | rust startup p50 | sys `list_walk` ns/elem | rust `list_walk` ns/elem | sys `churn` ns/op | rust `churn` ns/op |
|---|---|---|---|---|---|---|
| r1 | 108.2 ms | 125.9 ms | 2.25 | 14.91 | 4,175.1 | 23,521.6 |
| r2 | 132.9 ms | 162.3 ms | 2.33 | 15.79 | 4,867.7 | 25,066.5 |
| r3 | 105.7 ms | 126.8 ms | 2.26 | 15.26 | 4,186.3 | 23,535.0 |
| r4 | 106.7 ms | 137.1 ms | 2.23 | 15.03 | 4,241.9 | 24,224.8 |
| r5 | 107.1 ms | 124.8 ms | 2.24 | 15.15 | 4,229.4 | 22,472.0 |
| r6 | 109.1 ms | 135.0 ms | 2.49 | 23.16 | 4,650.0 | 25,933.9 |

Two rounds caught a load excursion and are kept as separate archived files rather than smoothed away:
- **r2** opened at 10.86 and closed at 15.88 (5-min 10.56 -> 11.78); every value in r2 rose on **both** sides (sys startup p50
  132.9 ms vs its 105–109 ms elsewhere; rust `churn` 25 066.9 ns/op).
- **r6** ran while the host climbed to 21.29/21.85 (the session maximum); it is the widest round on both sides (rust
  `list_walk` 23.2 ns/element, rust `gc_class/compile` 325.6 ms/op) and its pass wall times are the longest (25.5/26.3 s
  against ~20.4 s for a clean round). It is **not** dropped: the headline is the median of all six rounds and the per-round
  column is the stability evidence. The documented host asymmetry holds — the oracle is comparatively load-insensitive and the
  rust side absorbs the variance — so the quoted figure is the **median of the per-round ratios**, never a ratio of two medians.

## Withheld figures (none)
- **Zero marginals were withheld in m22**: all 60 rows (6 rounds x 2 sides x 5 workloads) are `status=ok`, and every per-unit
  case (`churn`, `list_walk`, `gc_class` `send_recv`/`ets_copy`/`compile`) reports its paired median in every round — 30 rust
  and 30 sys paired marginals, each clearing the driver's `3 x max(1 ms, 1.4826*MAD)` band. m21 withheld exactly one
  (`r7/rust churn`); m22 withheld none, which is why no row in `m22-vs-m21.txt` is short of a round.
- No capability gap (exit 3) occurred on either side, and no row is `skipped` or `failed`.
- The widest noise bands in m22 are the r6 rust margins (`list_walk` noise `±132.854 ms` on a 1158 ms paired median;
  `gc_class/ets_copy` `±60.837 ms` on 606 ms; `gc_class/send_recv` `±23.962 ms` on 537 ms) — all still far below their
  paired medians, so the figures stand; they are simply less precise than in a clean round.

## Notes the rows carry (verbatim in the JSON), and the caveats that travel with this record
- **Compile attribution (`gc_class/compile`)** — every rust `gc_class` row carries the guard's own note:
  `WARNING: code:which(compile)=/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/lib/compiler-10.0.7/ebin/compile.beam`
  — the **system OTP compiler**, not this tree's `bootstrap/lib/compiler/ebin`. So the `compile` figure is a
  *runtime-execution* comparison at a controlled compiler (how fast each runtime runs the same compile), never an
  in-tree-compiler measurement, and it can never be quoted without that attribution.
- **`gc_class/compile` is the slowest per-unit case in the record**: one `compile:file/2` of `lib/stdlib/src/ordsets.erl`
  costs ~240 ms/op on rust against ~16.6 ms/op on sys (N = 2 rust / 8 sys) — a 14.453x ratio, the widest here. It dominates
  a `gc_class` pass (~14–20 s of the ~20 s round) and is kept deliberately: it is the one GC class exercised through a real,
  long-running in-process workload, and dropping it would silently shrink the record's coverage.
- **`baseline`'s compute loop is a native floor, not throughput**: the loop is `lists:sum(lists:seq(1,1000000))`, dominated by
  natively served BIFs and by building a 1 000 000-element cons list. m22 reads rust **faster** than the oracle there
  (0.875x of sys wall time; 1.140x ops/s) — see `m22-vs-m21.txt` for why that row moved and why this record cannot attribute it.
- **`vm_memory_total_bytes` is accounting, not a footprint**: rust 7.25 MB vs sys 36.19 MB (0.200x) while the same run's OS
  peak RSS is rust 100.3 MB vs sys 135.2 MB (0.742x). Never read the 0.200x as a memory win; the footprint rows are in
  `m22-memory.md`.
- **`slice` is new in this record** and is the only N=1 (whole-process) comparison: every case is 0.984–1.046x of the
  oracle's wall time and 1.008–1.013x of its peak RSS — the first m-record in which a whole `beam_rt` process is at parity
  (m20's record read `slice` wall 1.410x). Each case checks its runtime's stdout against the expected constant, so a
  broken case fails the row instead of silently skewing a median.
- **The host is shared**: quote the ratios and the deterministic `case_*_ops`/`_elements` counters, never an absolute wall
  time as a constant. Absolute rust times move several-fold between sessions while the ratios move tens of percent.

## Memory rows
See `m22-memory.md` (rust vs sys peak RSS per workload, plus the independent `wait4`/`ru_maxrss` boot confirmation:
rust **66.03 MiB** vs sys **61.19 MiB** (1.079x) for `-noshell -eval halt().`, and rust 66.06 vs sys 61.65 MiB (1.071x) for
`-noshell -s init stop`, median of 5 samples each). The headline: the rust loop rows' peak RSS fell from ~96.7 MB (m21) to
~68.1–68.7 MB, i.e. from 1.43x of the oracle to 1.011–1.014x, because the boot no longer dominates those rows.

## Where the numbers live
| file | what it is |
|---|---|
| `m22-xrun-r{1..6}-{rust,sys}.json` | the twelve matched single-side `Report`s (the record) |
| `m22-xrun-ratios.txt` | the derived table: per-round values, per-round ratios, medians, counters, withheld audit |
| `m22-vs-m21.txt` | the per-row diff of the rust/sys ratio against m21, with the noise band and a verdict per row |
| `m22-memory.md` | the RSS rows and the independent boot confirmation |
| `m22-budgetprobe-all.json` / `.txt` | the supplementary `--runtime all` pass + its driver-printed budget verdicts |
| `m22-compare-cross-runtime.txt` | `bench compare m22-budgetprobe-all.json` — the driver's own cross-runtime diff of that pass |

Archiving normalisation (applied mechanically, disclosed here): `runtimes[].program`'s run-host worktree prefix becomes
`erts_rust/target/release/beam_rt`, any other worktree prefix is removed, and a scratch `/tmp/bench-scratch-...` prefix in
`notes` becomes `$TMPDIR/bench-scratch-...`. **No metric value was edited.** Every load figure above is verbatim.
