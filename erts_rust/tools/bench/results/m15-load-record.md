# m15 load record

Every run records `/proc/loadavg` (1/5/15-min) immediately before and after; one benchmark process
at a time. Session: **m15, 2026-10-17**, 14 logical cores, shared host. Raw logs:
`m15-xrun-load.log`, `m15-tier-load.log`, `m15-ext-load.log`.

| run | artifact | before (1/5/15) | after (1/5/15) |
|---|---|---|---|
| primary cross-runtime (`--full --runtime all`) | `m15-bench-xrun.json` | 4.91 / 5.23 / 5.85 | 2.65 / 3.54 / 4.89 |
| tier control — interpreter (`--quick --runtime rust`) | `m15-bench-xrun-interp.json` | 2.06 / 3.36 / 4.81 | 3.96 / 3.66 / 4.78 |
| tier control — default baseline JIT (`--quick --runtime rust`) | `m15-bench-xrun-quick.json` | 3.96 / 3.66 / 4.78 | 2.74 / 3.35 / 4.60 |
| external targets (`--full --runtime sys`) | `m15-bench-external.json` | 2.68 / 3.32 / 4.59 | 2.52 / 3.54 / 4.48 |
| `plb` (`--quick --runtime sys`) | `m15-bench-plb.json` | 2.40 / 3.50 / 4.46 | 2.27 / 3.17 / 4.25 |
| `emqx` (`--quick --runtime sys`) | `m15-bench-emqx.json` | 2.27 / 3.17 / 4.25 | 2.27 / 3.17 / 4.25 |

The tier controls were run back to back (interpreter first, then default JIT), so the shared
3.96 / 3.66 / 4.78 boundary row is the interpreter's *after* and the default JIT's *before*; the
2.74 / 3.35 / 4.60 row is the *after* of both.

## Superseded pass — not used by `RESULTS.md`

`m15-xrun-load.log` also holds an **earlier** primary cross-runtime pass taken **before** the rust
loop-count recalibration (`--full --runtime all`, 7.31 / 7.78 / 6.99 → 5.74 / 5.40 / 6.11). Its
report was discarded and re-run after the recalibration; **no figure in `RESULTS.md` comes from it**.

## Why the load matters

m15 was a comparatively quiet session (load ≈2–5) where m14 ran at 40–145. Absolute wall times and
throughputs are therefore **not** comparable to m14's — the m14→m15 improvement in absolute figures
is largely the quieter host. Read the ratios and the deterministic `case_*_ops/calls/elements`
counters in `../RESULTS.md` §3, never a single absolute wall time as a constant.

Every per-unit figure in this session comes from the **paired** estimator (`src/marginal.rs`): each
round samples the `N`-op pass and the 0-op pass back to back, the statistic is the median of the
paired differences, and a figure inside `3 × max(1 ms, 1.4826·MAD)` is withheld rather than
published (`../RESULTS.md` §3.4).
