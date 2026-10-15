# m12 — the `foreign` Θ(N³) regression is FIXED: re-derived A/B evidence

Milestone-12 companion to `b54-foreign-growth-curve.md` / `b55-foreign-regression-fix.md`: a **fresh,
independently re-derived** interleaved A/B of the m12 **HEAD** `beam_rt` against the **m10** baseline
binary on the `foreign` workload's `enc` case, run during the m12 measurement pass. It confirms the fix
landed on HEAD: at the calibrated loop counts the fixed runtime's startup-cancelled per-call marginal is
**at parity with the m10 pre-regression baseline** (HEAD/m10 ≈ 1.0 at N ≥ 25 000), where the **b54**
pre-fix record measured **5.00×** on the same case.

Every value below is raw (`date +%s%N` wall clock; `/proc/loadavg` recorded inline). Nothing is
estimated, and **no metric value inside any `results/*.json` artifact is altered by this file**.

## Sides named

* **fixed HEAD** (`head`, this section) = `beam_rt` built `--release` from the worktree tree
  `ee3e426db0` (the tree measured by the m12 pass; artifacts committed as `bdadd02333`).
  Binary: `erts_rust/target/release/beam_rt`. Default (baseline-JIT) tier.
* **m10** = `beam_rt` built `--release` from **`b4593818b3`** (the m10 milestone commit, an ancestor of
  HEAD; the last ~linear pre-regression point) in a **detached scratch worktree**
  (`git worktree add --detach "$TMPDIR/m10wt" b4593818b3`). The worktree path appears **only** as the
  literal `$TMPDIR/m10wt` placeholder in the committed artifacts.
* **`sys`** = the system ERTS oracle (OTP **29.1.1 / erts-17.1**), the system `erl`/`erlc`.
* Foreign sources: unchanged (cowboy HEAD `c965d1a`, `src/cowboy_bstr.erl` dated Oct 5 21:09; emqx HEAD
  `02eaa91831`). The staged `bench_foreign_driver.erl` in `$TMPDIR/m12exp` is byte-identical to b55's
  staged copy (`diff -q` passed), so this is the same program b55 measured.

## What was fixed (already landed on HEAD; owner outside this node)

A genuine **Θ(N³)** regression in the `foreign` benchmark path: the runner re-registered the engine's
whole never-cleared `refc_binaries` reservoir onto the running process **once per reduction slice**
(`crates/beam_rt/src/runner/run.rs`), and each registration was an **O(slots)** linear scan
(`crates/beam_process/src/process.rs`). Two commits close it:

1. **`07a5720639`** *beam_rt runner: register only the not-yet-registered refc-binary suffix per slice* —
   a per-engine `registered_payloads` cursor registers only the **new suffix** of the reservoir each slice.
2. **`7d84b12b44`** *beam_process: make off-heap-binary registration O(1) amortized* — an **O(1)**
   `owned_binary_index` `HashMap` replaces the O(slots) scan.

Both are ancestors of m12 HEAD. The pre-fix signature (a *diverging* head-vs-m10 curve) is gone below.

## Two confounds — read before the tables

1. **HEAD's `run` action now drives the full boot script (~2.07 s fixed).** Commit **`85ee33b2f8`**
   ("every non-`-version` CLI action drives the default boot script") makes `beam_rt run …` boot
   `start.boot` (kernel_sup / heart / logger / …) first. So **raw wall-clock is NOT comparable** between
   m10 and HEAD: HEAD's `N=0` startup is ≈2 090 ms vs m10's ≈18 ms. The per-call verdict therefore
   **must** use the startup-cancelled marginal `(wall(N) − wall(0)) / N`. This change is unrelated to the
   two-crate fix; its own, still-OPEN consequences are recorded in `m12-run-route-latency.md`.
2. **The pass ran on a shared, loaded host** (1-min loadavg 5.73–10.58 on 14 CPUs through the A/B). The
   ~2.1 s HEAD boot carries ±80–135 ms of jitter, which is small for `N ≥ 25 000` but comparable to the
   whole loop marginal at `N ≤ 10 000`. The small-`N` marginals below are correspondingly
   noise-dominated; the `N = 25 000 / 60 000` points are the load-robust ones.

## Primary A/B — `foreign` `enc/1` (raw `wall_ms`, interleaved, 5 passes/point)

Vehicle: one OS process per point, `$BIN -pa $D run $D/bench_foreign_driver.beam bench_foreign_driver:enc/1 $N`
(wall clock via `date +%s%N`; `/usr/bin/time` is absent on this host). **m10 and fixed HEAD alternate per
pass**, and each pass **brackets its loop points with two `N=0` startup runs** (`z0` before, `z1` after) so
the startup cancels against a contemporaneous baseline. The cancellation baseline is the **minimum of the
pass's two `N=0` runs**. `/proc/loadavg` before the pass: `6.72 8.10 10.26`; after: `5.73 7.92 9.77`.

Raw `wall_ms` and the per-pass 1-min loadavg:

| pass | m10 z0 / N5 000 / N10 000 / N25 000 / N60 000 / z1 | head z0 / N5 000 / N10 000 / N25 000 / N60 000 / z1 |
|---|---|---|
| 1 (load1 6.72→8.75) | 23 / 87 / 136 / 473 / 1 715 / 19 | 2 120 / 2 254 / 2 316 / 2 576 / 3 816 / 2 159 |
| 2 (load1 8.73→8.35) | 18 / 95 / 145 / 453 / 1 761 / 19 | 2 083 / 2 142 / 2 216 / 2 512 / 3 860 / 2 094 |
| 3 (load1 9.02→9.02) | 18 / 85 / 130 / 445 / 1 724 / 20 | 2 063 / 2 134 / 2 206 / 2 494 / 3 750 / 2 098 |
| 4 (load1 8.99→8.51) | 18 / 83 / 147 / 439 / 1 712 / 17 | 2 074 / 2 156 / 2 210 / 2 480 / 3 736 / 2 075 |
| 5 (load1 7.74→7.44) | 18 / 80 / 154 / 443 / 1 709 / 17 | 2 076 / 2 135 / 2 204 / 2 498 / 3 721 / 2 087 |

Startup-cancelled marginal `(wall(N) − min(z0,z1)) / N`, µs/call (per pass, then mean / median):

| `N` | m10 µs/call mean / median | head µs/call mean / median | head/m10 (mean) | head/m10 per-pass range |
|---|---|---|---|---|
| 5 000 | 13.64 / 13.4 | 16.20 / 14.2 | **1.19** | 0.77–1.97 (noise-dominated) |
| 10 000 | 12.46 / 12.7 | 14.72 / 13.6 | **1.18** | 0.93–1.68 (noise-dominated) |
| 25 000 | 17.31 / 17.08 | 17.15 / 17.16 | **0.99** | 0.96–1.01 |
| 60 000 | 28.44 / 28.27 | 28.23 / 28.12 | **0.99** | 0.97–1.02 |

**Honest read:** at the load-robust points the fixed curve is **no longer super-linear** and **tracks m10**
(head/m10 = **0.99 by mean and by median at both N = 25 000 and N = 60 000**). Both m10 and HEAD drift up
~1.8× from `N = 5 000` to `N = 60 000` (m10 13.4→28.3 µs, head 14.2→28.1 µs by median) — a **common-mode
intrinsic retention effect present in the m10 baseline too** (the known per-op `Nursery.retired` growth,
the same one documented in `RESULTS.md` §6.7), **not** the Θ(N³) registration regression: the regression's
signature was a *diverging* head-vs-m10 curve, and that is gone. The `N ≤ 10 000` head/m10 > 1 sits inside
the boot-jitter band (the whole loop marginal there is only ~100–200 ms against a ±100 ms boot swing).

**Contrast with the pre-fix record (b54, same driver, pre-fix HEAD `e1d51458c8`):** the growth curve rose
30.0 / 14.4 / 25.9 / 70.2 / **95.5** / **294.1** µs/call at `N` = 2 000 / 5 000 / 10 000 / 20 000 / 25 000 /
50 000 (a ~10× rise), and the pre-fix interleaved A/B measured head/m10 ≈ **4.6× @ N = 25 000** and
≈ **12.5× @ N = 60 000**; the b54 `--quick` driver A/B recorded **5.00×** on `emqx_base62_encode`. All of
that is gone at m12 HEAD.

## Other `foreign` sub-cases (head vs m10, `N = 25 000`, 3 passes interleaved)

| case | m10 µs/call (mean / median) | head µs/call (mean / median) | head/m10 (median) |
|---|---|---|---|
| `cap/1` (`bstr_capitalize_token`) | 24.36 / 24.48 | 22.64 / 22.76 | **0.93** |
| `dec/1` (`emqx_base62_decode`) | 17.00 / 17.08 | 15.19 / 14.92 | **0.87** |

Both non-`enc` sub-cases are **at or better than m10** (0.87–0.93×) — the fix is not `enc`-specific. Raw
walls, interleaved: m10 `z0`/`cap`/`dec` = 17/629/444, 18/630/445, 19/622/440; head = 2 090/2 659/2 463,
2 084/2 642/2 489, 2 094/2 665/2 455. (b55 measured the same two sub-cases at 0.99×/1.08× by median — the
same verdict: parity-plus.)

## b54/b55-compatible driver A/B (matched `N`, `--quick`, rust only)

Produced exactly as b54/b55 did (`BEAM_RT=$BIN bench run --quick --workloads foreign --runtime rust`),
interleaved, one pass per side, archived as the committed pair
`results/m12-foreign-ab-head.json` / `results/m12-foreign-ab-m10.json` (program paths normalised:
`erts_rust/target/release/beam_rt` and `$TMPDIR/m10wt/erts_rust/target/release/beam_rt`). Both rows are
`ok` (`4 ok, 0 unsupported`), and each side's stdout matched `N × per_call`.

`case_<name>_ns_per_call` at the case table's unchanged `n_rust` (matched `N`):

| case | `N` | m10 ns/call | head ns/call | **head/m10** | b54 (pre-fix) head/m10 |
|---|---|---|---|---|---|
| `emqx_base62_encode` | 25 000 | 18 286.00 | 16 628.83 | **0.91×** | **5.00×** |
| `bstr_to_lower` | 20 000 | 18 820.20 | 18 525.83 | **0.98×** | 1.24× |
| `bstr_capitalize_token` | 15 000 | 24 798.49 | 22 245.98 | **0.90×** | 1.22× |
| `emqx_base62_decode` | 15 000 | 17 587.02 | 15 341.64 | **0.87×** | 1.23× |

**The headline regression case (`emqx_base62_encode`) moves 5.00× → 0.91×** — i.e. the fixed HEAD is now
*faster* than the m10 baseline on every one of the four cases (0.87×–0.98×). The residual ±10 % spread is
the boot-jitter on a ~2.1 s startup subtraction (a ±100 ms boot swing is a large fraction of a
`N = 15 000–25 000` loop marginal) — confound 2. (m10's own startup is ~16 ms, so its marginal is clean;
the direct two-`N=0`-bracketed ladder above, which re-measures `N=0` in-pass, is the tighter estimate.)

## HEAD vs `sys` (system ERTS 29.1.1 / erts-17.1) — matched-`N` cross-runtime band

From the m12 **full cross-runtime run** (`results/m12-bench-xrun.json`, `--full --runtime all`, both
runtimes; `case_<name>_ns_per_call` is the startup-cancelled marginal on both sides):

| case | `N` sys / rust | sys ns/call | rust (HEAD) ns/call | **rust/sys** |
|---|---|---|---|---|
| `bstr_to_lower` | 1 500 000 / 20 000 | 419.77 | 23 429.88 | **55.8×** |
| `bstr_capitalize_token` | 1 500 000 / 15 000 | 361.57 | 22 901.06 | **63.3×** |
| `emqx_base62_encode` | 3 000 000 / 25 000 | 231.62 | 15 502.68 | **66.9×** |
| `emqx_base62_decode` | 3 000 000 / 15 000 | 383.96 | 14 919.34 | **38.9×** |

**Corrected band: `foreign` rust/sys = 38.9×–66.9×** (was **60.2×–367.1×** at m11, inflated by the
regression). Both sides named: `rust` = m12 HEAD `beam_rt` default (baseline-JIT) tier; `sys` = system ERTS
OTP 29.1.1 / erts-17.1. This is *still* a real per-call execution gap — it is the pre-existing `beam_rt`
per-call host-context cost, not this regression (the pre-regression m10 band was 52.4×–78.7×).

## `foreign` peak-RSS confound (do NOT read as like-for-like)

The `--quick` m10 row's whole-workload `peak_memory_bytes` is **80 093 184 B (76.4 MiB)** vs HEAD's
**462 934 016 B (441.4 MiB)**. That gap is **pure boot** — HEAD boots `start.boot` (≈394 MiB baseline),
m10 does not (≈15 MiB) — b55 proved this with a synthetic empty boot script — and it is **not** the fix.
The `foreign` per-case rust peak RSS is therefore loop- **and** boot-dependent; `slice` remains the
comparable per-process memory metric (`RESULTS.md` §6.7).

## Provenance / reproduction

```sh
# 1. Build both runtimes (repo root; the committed wrapper supplies a writable CARGO_HOME).
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt
# 2. The m10 binary in a detached scratch worktree.
git worktree add --detach "$TMPDIR/m10wt" b4593818b3
#    (then build -p beam_rt there with the committed wrapper)
# 3. Stage + compile the driver with the two real foreign sources (system erlc):
#    cowboy/src/cowboy_bstr.erl, emqx/apps/emqx/src/emqx_base62.erl, and the
#    src/workloads/foreign.rs DRIVER_SOURCE written verbatim as bench_foreign_driver.erl.
# 4. The interleaved ladder (one process per point): the enc/cap/dec cases at N,
#    each pass bracketed by two N=0 runs.
# 5. The b54/b55-compatible pair, one per side (matched N, --quick, rust only):
BEAM_RT=<head-or-m10-bin> erts_rust/target/release/bench run --quick --workloads foreign --runtime rust \
    --out-file erts_rust/tools/bench/results/m12-foreign-ab-<head|m10>.json
```

Raw probe log for this pass: the m12 session's `step4.log` (interleaved 5-pass ladder, the
`cap`/`dec` sub-case block, and the four driver A/B invocations), window 14:59:24Z–15:03:33Z.

## Verdict

1. **The two-crate fix works, re-confirmed at m12.** The fixed HEAD's startup-cancelled `foreign` `enc`
   marginal is at **parity with the m10 pre-regression baseline** (`head/m10 = 0.99` by mean *and* median at
   both `N = 25 000` and `N = 60 000`), and 0.87–0.93× (at-or-better) on the `cap`/`dec` sub-cases, versus
   **5.0×** for the pre-fix runtime (b54). The `--quick` driver A/B agrees on all four cases (0.87×–0.98×).
2. **Curve shape:** HEAD **tracks m10 at every ladder point** — the regression's diverging signature is
   gone. Both sides share a mild intrinsic ~1.8× drift from `N=5 000`→`60 000` (per-op `Nursery.retired`
   retention), a separate, pre-existing issue (`RESULTS.md` §6.7/§8 item 5).
3. **Raw wall-clock is confounded** by the unrelated boot-drive commit `85ee33b2f8` (~2.07 s fixed startup
   on **every** HEAD `run`); only the startup-cancelled marginals are comparable, and they are at-or-better
   than m10.
4. **The corrected cross-runtime band is 38.9×–66.9×** (`rust` HEAD default tier vs `sys` OTP 29.1.1) —
   down from m11's inflated 60.2×–367.1×. `RESULTS.md` §3.4 quotes this band, not the m11 one.
5. **Smallest honest caveat:** the two sides differ by **two** independent changes (this fix **and** the
   boot-drive `85ee33b2f8`); the fix's own effect is isolated by the startup-cancelled marginal and by
   b54's pre-fix numbers, both of which are reported here.
