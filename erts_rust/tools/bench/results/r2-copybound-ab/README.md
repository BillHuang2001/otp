# `copy_bound` paired A/B — the round-2 copy-path record

Raw artifacts (unedited) behind the copy-path round's figures. Read the ratios and the
`instructions` counts, never the absolute wall times: the host is shared and its 1-min load
ranged **17.71 → 6.84** across this run (each pass's load is in the job log; both sides are
interleaved so the drift cancels).

## Sides

| side | commit | binary |
|---|---|---|
| `base` | `2d0fe67808` (the round's starting commit) | `beam_rt`, release, default baseline-JIT tier |
| `head` | `4a80e6486a` (the round's merged HEAD) | same profile |

Both sides are `beam_rt` at its default tier; the `sys` column is the host oracle and is not
part of this A/B (these are within-`beam_rt` code-change figures, as in the repo's `m21`/`m22`
records).

## Protocol

Six rounds; within each round the two sides run back to back, and the order **alternates**
(`base,head` / `head,base`) so neither side owns a load trend.
`taskset -c 3 setsid -w perf stat -e instructions:u … bench run --quick --runtime rust
--workloads copy_bound --out-file <side><round>.json`, with `$BEAM_RT` selecting the binary.
`<side><round>.json` is the bench report, `<side><round>.stat` the `perf stat` block.

## Result

`cpu_core/instructions/u` per whole run (all four cases, deterministic):
`base` **127.50 G** (127.35–127.56) → `head` **100.45 G** (100.43–100.49) = **−21.22 %**,
disjoint in 6/6 rounds.

Per-case `case_<name>_ns_per_op`, median of six rounds (paired ratio = median of the per-round
`head/base` quotients):

| case | base | head | paired ratio |
|---|---|---|---|
| `etf_roundtrip` | 1 247.8 | 921.1 | **0.734** (−26.3 %) |
| `ets_copy` | 1 070.1 | 840.1 | **0.792** (−20.8 %) |
| `send_recv` | 4 770.9 | 2 885.5 | **0.608** (−39.2 %) |
| `send_recv_plain` | 2 029.8 | 1 324.5 | **0.648** (−35.2 %) |

The deterministic `case_*_ops` counters (500 000 / 250 000 / 90 000 / 300 000) are identical on
both sides, so the per-op normalisation is comparable.

`corpus-sweep.txt` is the folded 12-way differential sweep at `head`:
`total=3485 agree=3481 mismatch=0 unsupported=1 skipped=0 oracle_drift=3`, both ratchets clean.
