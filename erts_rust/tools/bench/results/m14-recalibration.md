# m14 — rust loop-count recalibration (Item 1 evidence)

This is the raw evidence behind the m14 recalibration of the `bench` rust-side loop
counts. It is a **driver-measurement** record, not a milestone record: the milestone
numbers are in `../RESULTS.md` and `m14-*`.

## The problem it fixes

At m13 the rust boot got ~2.9× cheaper (~2.1 s → ≈0.72 s), which left two rust marginals
**inside the boot's run-to-run jitter** — the workload emitted `marginal below timer
resolution; no ns_per_* metric` instead of a number:

| case | rust `N` (m13) | m13 `mean wall@N` | m13 `mean wall@0` | verdict |
|---|---|---|---|---|
| `copy_bound::etf_roundtrip` | 5 000 | 1 073.09 ms | 1 080.52 ms | no marginal |
| `list_walk` | 1 000 | 929.85 ms | 1 036.50 ms | no marginal |

## The rule adopted (also documented in each case table)

> `N` per runtime is chosen so the timed loop does enough work that its **marginal cost
> stands clear of that runtime's own run-to-run boot/startup jitter** (measured here at
> ≈±0.1 s on the rust boot, which itself is ≈0.7–1.5 s under load). The rust-side floor is
> **≥0.5 s of loop work** (≈5× that jitter); the cases whose marginal had actually fallen
> inside the band are targeted at **≳1.5 s**. The oracle's much smaller startup jitter
> (tens of ms) needs a correspondingly smaller marginal, so its counts stay far above its
> own noise. Cross-`N` comparison is kept honest by the per-op/per-element normalization —
> never compare raw `wall_ms` across two sides with different `N`.

Semantics NOT changed: the case programs, the deterministic stdout check, the
startup-cancelled formula, the `case_*_ops`/`case_*_elements` companion metrics, and the
honest-note behaviour when a marginal is below timer resolution.

## Calibration: measured rust per-unit cost on this host

Direct `beam_rt` / `erl` ladders (2026-10-16, interleaved N per process, startup cancels
in the difference; **min-of-reps** per N, `/proc/loadavg` 44–101), default baseline-JIT
tier unless stated:

| workload | `N` ladder | marginal/time | cost |
|---|---|---|---|
| `list_walk` (rust, default tier) | 20 000 / 40 000 / 80 000 walks | 625 / 1 239 / 2 632 ms | **~31–33 ns/element** |
| `list_walk` (rust, `BEAM_RT_TIER=interpreter`) | 1 000 / 4 000 walks | 550–980 / 1 500 ms | ~375–980 ns/element |
| `list_walk` (sys, OTP 29.1.1) | 50 000 / 100 000 / 200 000 walks | 311 / 497 / 678 ms | ~3.4–6.2 ns/element |
| `etf_roundtrip` (rust, default tier) | 8 000 / 16 000 / 32 000 ops | 431 / 859 / 1 683 ms | **~54 µs/op** |
| `etf_roundtrip` (sys) | 0.5 / 1.0 / 1.5 M ops | 417 / 702 / 1 156 ms | ~0.7–0.83 µs/op |
| `ets_copy` (rust) | 100 000 ops | 725 ms | ~7 µs/op |
| `send_recv` (rust) | 2 000 / 10 000 ops | 317 / 1 437 ms | ~145–160 µs/op |
| `send_recv_plain` (rust) | 2 000 / 10 000 ops | 107 / 1 152 ms | ~55–115 µs/op |
| `churn` (rust, `gc_bound`) | 2 000 / 5 000 churns | 258 / 894 ms | ~130–180 µs/op |

The ladder is **linear** in `N` over the ranges probed for `list_walk` (20k→80k) and
`etf_roundtrip` (8k→32k), so a larger `N` does not change the per-unit cost: the norm is
`N`-stable on these two cases at these counts.

## The new geometry

| workload | case | `n_rust` m13 → m14 | measured rust cost | loop work at m14 `N` |
|---|---|---|---|---|
| `list_walk` | `list_walk` | 1 000 → **50 000** | ~32 ns/element | ≈1.6 s |
| `copy_bound` | `etf_roundtrip` | 5 000 → **40 000** | ≈54 µs/op | ≈2.2 s |
| `copy_bound` | `ets_copy` | 100 000 → **200 000** | ≈7 µs/op | ≈1.4 s |
| `copy_bound` | `send_recv` | 2 000 → **10 000** | ≈150 µs/op | ≈1.5 s |
| `copy_bound` | `send_recv_plain` | 2 000 → **15 000** | ≈115 µs/op | ≈1.7 s |
| `gc_bound` | `churn` | 2 000 → **5 000** | ≈130–180 µs/op | ≈0.7–1.6 s |

`n_sys` is **unchanged** for every case (it resolved at m13 and it resolves again below);
every `n_sys > n_rust` invariant still holds.

## Proof of resolution — repeated driver runs (raw: `m14-recalibration.csv`)

`bench run --quick --runtime <rt> --workloads <w>`, repeated, **strictly one process at a
time**, `/proc/loadavg` recorded before/after every run. Each row is one independent run's
`mean wall@N`, `mean wall@0`, the resulting marginal and the reported marginal metric.

### `list_walk` (rust, `N` = 50 000 walks = 5×10⁷ elements)

| rep | wall@N | wall@0 | marginal | `ns_per_element` | load before→after |
|---|---|---|---|---|---|
| 1 | 3 451.35 | 2 222.74 | **+1 228.6** | 24.57 | 79.11 → 91.18 |
| 2 | 4 463.25 | 1 818.62 | **+2 644.6** | 52.89 | 91.18 → 91.94 |
| 3 | 3 754.98 | 1 874.71 | **+1 880.3** | 37.61 | 91.94 → 97.49 |
| 4 | 4 412.68 | 2 119.69 | **+2 293.0** | 45.86 | 97.49 → 108.26 |

### `list_walk` (sys, `N` = 100 000 walks = 10⁸ elements)

| rep | wall@N | wall@0 | marginal | `ns_per_element` | load before→after |
|---|---|---|---|---|---|
| 1 | 2 088.88 | 625.30 | **+1 463.6** | 14.64 | 108.26 → 115.89 |
| 2 | 1 685.54 | 631.92 | **+1 053.6** | 10.54 | 115.89 → 115.74 |
| 3 | 1 707.46 | 521.80 | **+1 185.7** | 11.86 | 115.74 → 117.40 |
| 4 | 1 604.38 | 630.22 | **+974.2** | 9.74 | 117.40 → 117.55 |

### `copy_bound` rust (loads 108.91 → 135.59 during the four rust runs)

| rep | case | `N` | wall@N | wall@0 | marginal | `ns_per_op` |
|---|---|---|---|---|---|---|
| 1 | `etf_roundtrip` | 40 000 | 9 762.22 | 2 161.68 | **+7 600.5** | 190 013.60 |
| 1 | `ets_copy` | 200 000 | 3 863.67 | 1 980.34 | **+1 883.3** | 9 416.61 |
| 1 | `send_recv` | 10 000 | 4 019.93 | 2 323.19 | **+1 696.7** | 169 674.62 |
| 1 | `send_recv_plain` | 15 000 | 2 803.65 | 1 994.33 | **+809.3** | 53 954.48 |
| 2 | `etf_roundtrip` | 40 000 | 7 864.55 | 2 204.07 | **+5 660.5** | 141 512.00 |
| 2 | `ets_copy` | 200 000 | 5 466.36 | 1 965.10 | **+3 501.3** | 17 506.28 |
| 2 | `send_recv` | 10 000 | 4 863.56 | 2 481.71 | **+2 381.9** | 238 185.63 |
| 2 | `send_recv_plain` | 15 000 | 3 248.18 | 1 551.06 | **+1 697.1** | 113 140.96 |
| 3 | `etf_roundtrip` | 40 000 | 7 589.87 | 2 393.42 | **+5 196.4** | 129 911.22 |
| 3 | `ets_copy` | 200 000 | 4 079.19 | 2 042.43 | **+2 036.8** | 10 183.79 |
| 3 | `send_recv` | 10 000 | 3 701.80 | 1 519.24 | **+2 182.6** | 218 255.65 |
| 3 | `send_recv_plain` | 15 000 | 2 733.33 | 1 782.35 | **+951.0** | 63 398.77 |
| 4 | `etf_roundtrip` | 40 000 | 17 185.65 | 1 781.45 | **+15 404.2** | 385 104.98 |
| 4 | `ets_copy` | 200 000 | 4 687.00 | 2 201.87 | **+2 485.1** | 12 425.69 |
| 4 | `send_recv` | 10 000 | 3 032.22 | 1 441.54 | **+1 590.7** | 159 067.66 |
| 4 | `send_recv_plain` | 15 000 | 2 727.74 | 1 812.49 | **+915.3** | 61 016.95 |

### `copy_bound` sys (all four cases resolvable in all three runs)

| rep | `etf_roundtrip` (1.5 M) | `ets_copy` (2 M) | `send_recv` (0.5 M) | `send_recv_plain` (1 M) | load before→after |
|---|---|---|---|---|---|
| 1 | +1 537.1 ms / 1 024.7 ns | +943.9 ms / 471.96 ns | +1 081.4 ms / 2 162.8 ns | +897.6 ms / 897.63 ns | 108.91 → 112.95 |
| 2 | +1 186.1 ms / 790.73 ns | +902.2 ms / 451.11 ns | +987.3 ms / 1 974.5 ns | +880.2 ms / 880.15 ns | 112.95 → 119.16 |
| 3 | +1 479.8 ms / 986.54 ns | +750.8 ms / 375.42 ns | +958.8 ms / 1 917.6 ns | +714.3 ms / 714.35 ns | 119.16 → 96.43 |

### `gc_bound` (rust `N` = 5 000 churns; sys `N` = 200 000)

| runtime | rep | wall@N | wall@0 | marginal | `ns_per_op` | load before→after |
|---|---|---|---|---|---|---|
| rust | 1 | 2 447.81 | 1 349.16 | **+1 098.7** | 219 730.47 | 96.43 → 83.98 |
| rust | 2 | 2 449.81 | 1 673.38 | **+776.4** | 155 285.15 | 83.98 → 75.79 |
| rust | 3 | 2 436.74 | 1 954.46 | **+482.3** | 96 454.96 | 75.79 → 73.55 |
| rust | 4 | 2 731.84 | 1 404.98 | **+1 326.9** | 265 371.83 | 73.55 → 74.78 |
| sys | 1 | 2 637.34 | 457.42 | **+2 179.9** | 10 899.55 | 74.78 → 70.26 |
| sys | 2 | 2 079.81 | 155.34 | **+1 924.5** | 9 622.39 | 70.26 → 64.76 |
| sys | 3 | 1 432.36 | 163.85 | **+1 268.5** | 6 342.54 | 64.76 → 61.42 |

**Verdict: 43/43 runs resolved.** Every rust marginal is positive and ≥0.48 s (24 of them
≥0.78 s, the two recalibrated cases 1.2–15.4 s), i.e. 5×–150× the ≈±0.1 s boot jitter
band; every `sys` marginal is positive and ≥0.71 s (all ≫ the oracle's tens-of-ms startup
jitter). No run emitted the `marginal below timer resolution` note.

Absolute per-op values here are **load-inflated** (the proof runs sat at load 73–136, and
the rust boot itself measured 1.4–2.5 s vs ≈0.55–0.72 s on a quiet host): read the
marginals as *resolvability* evidence, and the m14 milestone record (`m14-bench-xrun.json`,
`../RESULTS.md` §3) for quotable numbers.

## Files

- `m14-recalibration.csv` — the machine-readable pivot of the 43 proof runs
  (`workload,runtime,rep,case,wall_mean_ms,startup_mean_ms,marginal_ms,ns_per_unit,units,load_before,load_after`).
- The crate change itself: `../src/workloads/{list_walk,copy_bound,gc_bound}.rs` (the case
  tables' calibration doc comments state the rule and the measured costs; the in-file tests
  `the_rust_loop_count_clears_the_boot_jitter_band` /
  `the_rust_loop_counts_clear_the_boot_jitter_band` pin the new counts).
