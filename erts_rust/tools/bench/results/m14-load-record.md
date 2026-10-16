# m14 load record

Every `bench`/`beam_rt`/`erl` measurement was run **strictly one process at a time**, with
`/proc/loadavg` sampled before and after each run. Raw logs: `m14-xrun-load.log`,
`m14-tier-load.log`, `m14-ext-load.log`, `m14-ab-load.log`.

| run | 1-min loadavg before → after | artifact |
|---|---|---|
| primary cross-runtime `--full --runtime all` | 56.76 → 73.66 | `m14-bench-xrun.json` |
| tier control, interpreter `--quick --runtime rust` | 74.70 → 43.32 | `m14-bench-xrun-interp.json` |
| tier control, default JIT `--quick --runtime rust` | 43.32 → 40.66 | `m14-bench-xrun-quick.json` |
| external `--full --runtime sys` (erlc/elixirc/cowboy/khepri) | 39.89 → 44.09 | `m14-bench-external.json` |
| `plb` `--quick --runtime sys` | 43.44 → 59.20 | `m14-bench-plb.json` |
| `emqx` `--quick --runtime sys` | 59.20 (unchanged) | `m14-bench-emqx.json` |
| foreign A/B r1 head / m13 / m10 | 88.99→87.22 / 87.22→94.80 / 94.80→95.45 | `m14-ab-*-r1.json` |
| foreign A/B r2 head / m13 / m10 | 95.45→101.54 / 101.54→105.91 / 105.91→102.71 | `m14-ab-*-r2.json` |
| foreign A/B r3 head / m13 / m10 | 102.71→120.84 / 120.84→138.08 / 138.08→145.18 | `m14-ab-*-r3.json` |

The host is shared and heavily loaded (14 cores). The primary pass and the A/B are therefore
**contended** runs: read ratios and the deterministic `case_*_ops/calls/elements` counters, and
prefer the interleaved A/B for binary-vs-binary comparisons. Host: 14 logical cores;
`/usr/bin/time` absent (the harness uses `date +%s%N` and `/proc/<pid>/status`).
