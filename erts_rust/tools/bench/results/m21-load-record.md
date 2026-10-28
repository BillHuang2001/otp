# m21 host/load record — the GC/allocator workstream's "before" performance record (2026-10-27)

Session: **m21**, 2026-10-27 (12:58–13:05 UTC), 14 logical cores, **shared host**. `/proc/loadavg`
(1/5/15-min) was read immediately before and after every pass and exactly one benchmark process ran
at a time. The 1-min figure ranged **5.65 – 19.57** over the session: it opened while the host was
still loaded (19.57, falling) and closed in a quieter window, so m21 is a **mid-load** session and its
absolute wall times are load-contaminated on both sides (compare m19's 0.54–4.53 and m17's 3–92).

**What this record is for:** it is the *before* measurement for the GC/allocator workstream, not a new
headline. Its four workload ids (`baseline`, `gc_bound`, `gc_class`, `list_walk`) are the group that
workstream moves, and a later A/B is judged against these numbers — never by chaining a ratio with an
A/B (`../BOTTLENECKS.md` §1.1).

Sides: `sys` = the system ERTS on `PATH` — **Erlang/OTP 29.1.2 / erts-17.1.1**
(`/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl`,
`version=29`); `rust` = `erts_rust/target/release/beam_rt` at its default baseline-JIT tier
(`version=30`). **Note the oracle moved:** earlier records (m17–m19; the artifact-less m20 named in `BOTTLENECKS.md` §0 has no reports here) named OTP **29.1.1 / erts-17.1**;
this host's `erl` is **29.1.2 / erts-17.1.1**. Both sides, every pass, are recorded in
`m21-xrun-ratios.txt` §G; ratios are therefore vs *this* oracle, not the one m20 quoted.

## Command shape (one matched round = two single-side passes, alternating order)

```sh
WL=baseline,gc_bound,gc_class,list_walk
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --verbose --runtime rust \
    --workloads $WL --out-file erts_rust/tools/bench/out/m21-xrun-gc-r<N>-rust.json
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --verbose --runtime sys \
    --workloads $WL --out-file erts_rust/tools/bench/out/m21-xrun-gc-r<N>-sys.json
```

Side order alternated every round (r1 rust→sys, r2 sys→rust, r3 rust→sys, r4 sys→rust,
r5 rust→sys, r6 sys→rust, r7 rust→sys). **Seven** matched rounds (one more than the three required)
plus one supplementary `--runtime all` budget pass. Raw boundary log: `m21-xrun-load.log`.

| run | artifact(s) | pin | `/proc/loadavg` 1-min before → after | wall |
|---|---|---|---|---|
| `r1` rust | `m21-xrun-gc-r1-rust.json` | `taskset -c 3` | 19.57 → 16.30 | 18 s |
| `r1` sys | `m21-xrun-gc-r1-sys.json` | `taskset -c 3` | 16.30 → 13.86 | 14 s |
| `r2` sys | `m21-xrun-gc-r2-sys.json` | `taskset -c 3` | 13.86 → 13.00 | 15 s |
| `r2` rust | `m21-xrun-gc-r2-rust.json` | `taskset -c 3` | 13.00 → 10.91 | 16 s |
| `r3` rust | `m21-xrun-gc-r3-rust.json` | `taskset -c 3` | 10.91 → 9.09 | 16 s |
| `r3` sys | `m21-xrun-gc-r3-sys.json` | `taskset -c 3` | 9.09 → 8.27 | 14 s |
| `r4` sys | `m21-xrun-gc-r4-sys.json` | `taskset -c 3` | 8.27 → 7.76 | 14 s |
| `r4` rust | `m21-xrun-gc-r4-rust.json` | `taskset -c 3` | 7.76 → 6.76 | 16 s |
| `r5` rust | `m21-xrun-gc-r5-rust.json` | `taskset -c 3` | 6.76 → 7.19 | 17 s |
| `r5` sys | `m21-xrun-gc-r5-sys.json` | `taskset -c 3` | 7.19 → 7.34 | 15 s |
| `r6` sys | `m21-xrun-gc-r6-sys.json` | `taskset -c 3` | 5.65 → 7.00 | 15 s |
| `r6` rust | `m21-xrun-gc-r6-rust.json` | `taskset -c 3` | 7.00 → 6.35 | 20 s |
| `r7` rust | `m21-xrun-gc-r7-rust.json` | `taskset -c 3` | 6.35 → **12.44** | 34 s |
| `r7` sys | `m21-xrun-gc-r7-sys.json` | `taskset -c 3` | 12.44 → 13.90 | 16 s |
| budget pass (`--runtime all`) | `m21-budgetprobe-all.json` + `.txt` | `taskset -c 3` | 13.84 → 9.58 | 32 s |

Rounds 6–7 were added after a quieter window appeared (the first five ran while the host drained from
19.57 to ~7): they are kept as **separate archived files** rather than replacing rounds 1–5, because
the headline is the median of all seven and the per-round spread is the stability evidence.

## What the load did to each side (per-round stability evidence)

| round | sys startup p50 | rust startup p50 | sys `list_walk` ns/elem | rust `list_walk` ns/elem | sys `churn` ns/op | rust `churn` ns/op |
|---|---|---|---|---|---|---|
| r1 | 106.9 ms | 187.1 ms | 2.41 | 16.01 | 4 183.8 | 28 396.0 |
| r2 | 132.4 ms | 163.8 ms | 2.26 | 15.12 | 4 246.1 | 25 612.9 |
| r3 | 105.6 ms | 162.6 ms | 2.31 | 15.07 | 4 189.3 | 24 995.9 |
| r4 | 106.0 ms | 163.0 ms | 2.21 | 15.15 | 4 203.0 | 25 757.9 |
| r5 | 125.9 ms | 161.5 ms | 2.27 | 16.75 | 4 256.5 | 25 375.0 |
| r6 | 108.3 ms | 177.9 ms | 2.23 | **31.82** | 4 220.3 | 25 902.7 |
| r7 | 122.8 ms | **385.7 ms** | 2.69 | 19.00 | 4 281.3 | **withheld** |

The per-round ratios (`rust/sys`) stay tight wherever the host did not spike — `churn` 5.96–6.79 over
six rounds, `ets_copy` 6.40–7.18 over seven, `send_recv` 5.51–6.43 (one 7.90 at the r7 spike) — and
the two spike-affected rounds are visible in the ratio column rather than smoothed away: r6/rust
`list_walk` read 31.82 ns/element (14.27×) and r7/rust read 385.7 ms startup (3.14×) while the sys
side barely moved. That asymmetry is the documented behaviour of this host: **the oracle is
comparatively load-insensitive and the rust side absorbs the variance**, so the quoted figure is the
**median of the per-round ratios**, never a ratio of two medians from different windows.

## Withheld figures (exactly one)

- `r7/rust gc_bound/case_churn_ns_per_op` — withheld by the driver: *"case churn: marginal below timer
  resolution: paired median=610.776 ms min=90.393 ms max=828.351 ms noise=±322.577 ms over 3
  round(s)"*. The r7/rust pass caught the load spike (6.35 → 12.44) and its paired differences never
  cleared the noise band, so **no `churn` figure exists for round 7**; `gc_bound`'s median ratio is
  therefore over **six** rounds (5.962–6.787, median **6.080**), and its rust value is the median of
  six values (25 685.39 ns/op). Everything else reports its paired median.
- All 56 rows (7 rounds × 2 sides × 4 workloads) are `status=ok`; no row is `skipped` or `failed`, and
  no capability gap (exit 3) occurred on either side.

## Notes the rows carry (verbatim in the JSON)

- `gc_class`'s **in-process compile case is the slowest per-unit case in the record**: one
  `compile:file/2` of `lib/stdlib/src/ordsets.erl` costs **~252 ms/op on rust vs ~17 ms/op on sys**
  (N = 2 rust / 8 sys) — a **14.87×** ratio, the widest here — and it is the main reason a `gc_class`
  pass takes ~14–20 s (34 s on the loaded r7/rust leg) against ~14–16 s for the other three workloads
  together. It is kept in the record deliberately: it is the one GC class exercised through a real,
  long-running in-process workload, and dropping it would silently shrink the record's coverage.
- **Compile attribution** (the guard's own `WARNING:` note, present in every rust `gc_class` row):
  `code:which(compile)` = `…/erlang-29.1.2/lib/erlang/lib/compiler-10.0.7/ebin/compile.beam` — the
  **system OTP compiler**, not this tree's `bootstrap/lib/compiler/ebin`. The `compile` figure is
  therefore a *runtime-execution* comparison at a controlled compiler, never an in-tree-compiler
  measurement. The row also notes that the 0-op startup pass prints no `which=<path>` line (it
  compiles nothing), which is expected and not a missing attribution.
- `gc_class`'s payload is ~390 words, deliberately above the default 233-word nursery and rebuilt every
  round, so a collection really is admitted — that is what makes these rows GC rows rather than
  another copy measurement.
- `baseline`'s compute loop is dominated by natively-served `lists:seq/2` + `lists:sum/1`, so its
  ~1.11× ratio reads as the **native floor**, not execution throughput; the per-unit execution signals
  are `list_walk`/`gc_bound`/`gc_class` above.
- `vm_memory_total_bytes` is the runtime's own accounting (`erlang:memory(total)`), **not** a footprint
  and **not** comparable with the OS RSS row: rust 9.32 MB vs sys 36.2 MB (0.257×) while the rust OS
  peak RSS for the same run is 128.1 MB vs sys 135.2 MB (0.947×).

## Memory rows (VmHWM polling, bytes)

`baseline` and the three loop workloads report `peak_memory_bytes` / `case_*_peak_rss_bytes`; **`slice`
was not in this record's workload set**, so m21 carries no like-for-like (N=1) footprint row — the loop
rows are boot-dominated on the rust side (a fresh `beam_rt` process boots at ~96 MB regardless of the
loop) and must not be read as a per-workload memory comparison.

| workload | rust (median) | sys (median) | rust/sys |
|---|---|---|---|
| `baseline` | 128 106 496 | 135 168 000 | **0.947** |
| `gc_bound` (`case_churn_peak_rss_bytes`) | 96 722 944 | 67 661 824 | 1.429 |
| `list_walk` | 96 694 272 | 67 600 384 | 1.430 |
| `gc_class` (`case_send_recv_peak_rss_bytes`) | 193 597 440 | 67 624 960 | 2.862 |

`baseline`'s rust peak sits **below** the oracle's (0.947×) — the boot footprint, reproduced across all
seven rounds (0.947–0.948). `gc_class`'s 2.86× is the **`send_recv` case's own** peak (193 568 768 B on
rust), i.e. the documented retained-memory shape of a two-process round trip of a ~390-word payload
(`BOTTLENECKS.md` B8: the nursery grows and is never shrunk) — not the compile case, whose own peak is
206 528 512 B rust vs 78 745 600 B sys (2.62×), and not `ets_copy` (97 177 600 B rust, 1.44×).

## Where the numbers live

| file | what it is |
|---|---|
| `m21-xrun-gc-r{1..7}-{rust,sys}.json` | the fourteen matched single-side `Report`s (the record) |
| `m21-xrun-ratios.txt` | the derived table: per-round values, per-round ratios, medians, counters, withheld audit |
| `m21-xrun-load.log` | `/proc/loadavg` at every pass boundary, with rc and pass wall time |
| `m21-budgetprobe-all.json` / `.txt` | the supplementary `--runtime all` pass + its driver-printed budget verdicts (ratio rows need both sides in one report) |
| `m21-compare-cross-runtime.txt` | `bench compare m21-budgetprobe-all.json` — the driver's own cross-runtime diff (supplementary pass, ordering-biased by construction) |

Archiving normalisation (applied mechanically, disclosed here): `runtimes[].program`'s run-host worktree
prefix → `erts_rust/target/release/beam_rt`, any other worktree prefix removed, and a scratch
`/tmp/bench-scratch-…` (or `/tmp/genesis/task_<id>/bench-scratch-…`) prefix in `notes` →
`$TMPDIR/bench-scratch-…`. **No metric value was edited.**
The load log's run labels were stored without their separator and are restored as `r<N>/<side>`; its
`running/total` process-count field and the driver's free-form per-pass output are not reproduced (the
driver's notes live inside each JSON). Every load figure in it is verbatim.
