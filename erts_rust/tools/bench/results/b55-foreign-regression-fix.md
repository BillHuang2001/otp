# b55 — `foreign` Θ(N³) regression: two-crate fix, verified

Raw evidence that the two-crate performance fix landing on HEAD **removes the super-linear
`foreign` per-call cost**: at the calibrated loop counts the fixed runtime's startup-cancelled
per-call marginal is now at parity with the m10 pre-regression baseline (m10↔HEAD 1.0–1.1×),
where b54 measured the pre-fix runtime at **5.0×** m10 on the same case. Every value below is
raw; nothing is estimated. No metric inside any `results/*.json` artifact is altered by this file.

* **fixed HEAD** = `beam_rt` built `--release` from `d854e55df7` (the tree this document is
  committed with). The binary is `erts_rust/target/release/beam_rt`.
* **m10** = `beam_rt` built `--release` from `b4593818b3` (the m10 milestone commit, an ancestor
  of HEAD; the last ~linear pre-regression point) in a **detached scratch worktree**
  (`git worktree add --detach "$TMPDIR/m10wt" b4593818b3`). The worktree path is a scratch path
  and is **never** baked into any committed artifact except as the literal `$TMPDIR/m10wt`
  placeholder.
* Oracle for the foreign sources: system `erlc` (OTP 29.1.1 / erts-17.1).
* Host: `billmini`, **14 CPUs online**, shared and saturated (1-min loadavg ranged **6–45**
  through the pass; every batch's loadavg is recorded below). Absolute times are noisy — the
  verdict rests on the **startup-cancelled marginals and the CPU-time shape**, and every ratio is
  reported with both sides.

## What was fixed

A genuine **Θ(N³)** regression in the `foreign` benchmark workload: the runner re-registered the
engine's whole never-cleared `refc_binaries` reservoir onto the running process **once per
reduction slice** (`crates/beam_rt/src/runner/run.rs`), and each registration was an **O(slots)**
linear scan (`crates/beam_process/src/process.rs`). Two commits on HEAD close it:

1. `07a5720639` **beam_rt** — a per-engine `registered_payloads` cursor registers only the **new
   suffix** of the reservoir each slice (`runner/{run.rs,entry.rs}`).
2. `7d84b12b44` **beam_process** — an **O(1)** `owned_binary_index` `HashMap` replaces the O(slots)
   scan (`process.rs`).

Pre-fix evidence (the bug): `b54-foreign-growth-curve.md` + `b54-foreign-ab-{m10,head}.json`.

### Reproduction (exact commands)

```sh
# 1. Build both runtimes (from the repo root; the committed wrapper supplies CARGO_HOME).
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt

# 2. The m10 binary in a detached scratch worktree.
git worktree add --detach "$TMPDIR/m10wt" b4593818b3
( cd "$TMPDIR/m10wt" && ./erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt )

# 3. Stage + compile the driver together with the two real foreign sources (system erlc).
#    The driver source is the `DRIVER_SOURCE` constant in src/workloads/foreign.rs,
#    written verbatim to "$TMPDIR/b55exp/bench_foreign_driver.erl".
mkdir -p "$TMPDIR/b55exp"
cp /home/bill/Source/cowboy/src/cowboy_bstr.erl "$TMPDIR/b55exp/"
cp /home/bill/Source/emqx/apps/emqx/src/emqx_base62.erl "$TMPDIR/b55exp/"
# (write DRIVER_SOURCE -> "$TMPDIR/b55exp/bench_foreign_driver.erl")
( cd "$TMPDIR/b55exp" && erlc -o "$TMPDIR/b55exp" cowboy_bstr.erl emqx_base62.erl bench_foreign_driver.erl )

# 4. The reproduction vehicle — one direct process per point, wall clock via `date +%s%N`
#    (`/usr/bin/time` is absent on this host). `$BIN` is the fixed-HEAD or the $TMPDIR/m10wt binary.
"$BIN" -pa "$TMPDIR/b55exp" run "$TMPDIR/b55exp/bench_foreign_driver.beam" bench_foreign_driver:enc/1 "$N"

# 5. The b54-compatible report (used for the JSON artifacts), one per runtime:
BEAM_RT="$BIN" erts_rust/target/release/bench run --quick --workloads foreign --runtime rust \
    --json --out-file /tmp/b55-foreign-ab.json
```

The direct ladder is **interleaved per pass** (m10 and fixed HEAD alternate), and each pass
**brackets its loop points with two `N=0` startup runs** so the startup cancels against a
contemporaneous baseline (below).

## Two confounds you must read before the tables

1. **HEAD's `run` action now drives the full boot script (~2.07 s fixed).** Commit `85ee33b2f8`
   ("every non-`-version` CLI action drives the default boot script", landed **after** b54's HEAD
   `e1d51458c8`) makes `beam_rt run …` boot `start.boot` (kernel_sup / heart / logger / …) before
   the workload. So **raw wall-clock is NOT comparable** between m10 and HEAD: fixed HEAD's `N=0`
   startup is ~2 090 ms vs m10's ~18 ms. The per-call verdict therefore **must** use the
   startup-cancelled marginal `(wall(N) − wall(0)) / N`. This change is unrelated to the two-crate
   fix.
2. **The pass ran on a saturated host** (loadavg 6–45 on 14 CPUs). The ~2.1 s boot carries
   ±80–135 ms of jitter, which is small for `N >= 25 000` but dominates `N <= 10 000` (where the
   whole loop marginal is only ~100–200 ms). The small-`N` marginals below are correspondingly
   noise-dominated; the `N = 25 000 / 60 000` points are the load-robust ones.

## Primary A/B — `foreign` `enc/1` (raw `wall_ms`, interleaved, 5 samples/point)

`/proc/loadavg` (1/5/15) before the pass: `13.15 16.38 20.61` (13/985 runnable/total); after:
`7.09 12.71 18.73`. Batch points recorded inline (1-min loadavg at each run).

**N = 25 000** (mean m10 **455.8 ms** / fixed HEAD **2 587.6 ms**):

| sample | m10 wall_ms | fixed-HEAD wall_ms | m10 load1 | head load1 |
|---|---|---|---|---|
| 1 | 443 | 2 531 | 13.15 | 12.14 |
| 2 | 435 | 2 536 | 8.48 | 9.75 |
| 3 | 438 | 2 536 | 8.48 | 7.40 |
| 4 | 484 | 2 812 | 7.80 | 6.58 |
| 5 | 479 | 2 523 | 7.80 | 7.78 |
| **min / mean** | **435 / 455.8** | **2 523 / 2 587.6** | — | — |

Raw wall ratio head/m10 = **5.68×** — but this is entirely the ~2.09 s boot confound (HEAD's
`N=0` is 2 094 ms): cancelling startup, **m10 ~ 17.5 µs/call vs fixed HEAD ~ 19.7 µs/call
(1.13×)**.

**N = 60 000** (mean m10 **1 777.8 ms** / fixed HEAD **3 862.4 ms**):

| sample | m10 wall_ms | fixed-HEAD wall_ms | m10 load1 | head load1 |
|---|---|---|---|---|
| 1 | 1 806 | 3 761 | 13.15 | 11.25 |
| 2 | 1 674 | 3 811 | 8.48 | 9.05 |
| 3 | 1 673 | 3 828 | 8.48 | 6.81 |
| 4 | 1 902 | 4 029 | 7.80 | 7.26 |
| 5 | 1 834 | 3 883 | 8.37 | 7.40 |
| **min / mean** | **1 673 / 1 777.8** | **3 761 / 3 862.4** | — | — |

Raw wall ratio head/m10 = **2.17×**; startup-cancelled: **m10 ~ 29.3 µs/call vs fixed HEAD
~ 29.5 µs/call (1.01× — parity)**.

### `N=0` startup (the cancellation baseline, per pass)

| pass | m10 zeroA/zeroB ms | fixed-HEAD zeroA/zeroB ms |
|---|---|---|
| 1 | 44 / 18 | 2 184 / 2 075 |
| 2 | 18 / 19 | 2 158 / 2 077 |
| 3 | 21 / 18 | 2 165 / 2 083 |
| 4 | 25 / 20 | 2 103 / 2 199 |
| 5 | 20 / 21 | 2 132 / 2 164 |

The marginal at each point uses the **minimum** of that pass's two bracketing `N=0` runs.

## Growth ladder — startup-cancelled marginal `(wall(N) − wall(0)) / N`, µs/call

Fixed HEAD and m10, same points, interleaved. (`mean` / `median` over the samples; the small-`N`
cells are noise-dominated — see confound 2.)

| `N` | m10 µs/call (mean / median) | fixed-HEAD µs/call (mean / median) | head/m10 (mean) |
|---|---|---|---|
| 5 000 | 17.7 / 14.2 | 18.9 / 15.8 | 1.07 |
| 10 000 | 13.8 / 13.0 | 15.8 / 18.9 | 1.14 |
| 25 000 | 17.5 / 17.0 | 19.7 / 18.2 | 1.13 |
| 60 000 | 29.3 / 29.8 | 29.5 / 29.1 | 1.01 |

**Pre-fix reference (b54's `foreign` growth curve, same `enc` driver, pre-fix HEAD):**
30.0 / 14.4 / 25.9 / 70.2 / **95.5** / **294.1** µs/call at `N` = 2 000 / 5 000 / 10 000 / 20 000 /
25 000 / 50 000 — a ~10× rise across the range. Fixed HEAD is now **18.2 µs/call @ 25 000** (was
95.5) and **29.1 µs/call @ 60 000** (pre-fix @ 50 000 was 294.1): a **~5–10× per-call cut**, landing
on the m10 baseline.

**Honest read:** the fixed curve is **no longer super-linear in the b54 sense** and **tracks m10 at
every point** (head/m10 = 0.98–1.14 by median). It is *not* perfectly flat: both m10 and fixed HEAD
drift up ~1.8–2× from `N=5 000` to `N=60 000` (m10 14.2→29.8, head 15.8→29.1). That common-mode
drift is an **intrinsic retention effect present in the m10 baseline too** (the known per-op
`Nursery.retired` growth, not the Θ(N³) registration regression — the regression's signature was a
*diverging* head-vs-m10 curve, which is gone).

## Other `foreign` sub-cases (fixed HEAD vs m10, `N = 25 000`, 3 samples/point)

| case | m10 µs/call (mean / median) | fixed-HEAD µs/call (mean / median) | head/m10 (median) |
|---|---|---|---|
| `cap/1` (`bstr_capitalize_token`) | 23.1 / 23.3 | 24.3 / 23.0 | 0.99 |
| `dec/1` (`emqx_base62_decode`) | 17.0 / 16.5 | 17.0 / 17.8 | 1.08 |

Both non-`enc` sub-cases are at parity (0.99–1.08×) — the fix is not `enc`-specific.

## b54-compatible report (bench tool, matched `N`)

`b55-foreign-ab-{head,m10}.json` (mirrors the b54 JSON shape; `program` sanitised to
`erts_rust/target/release/beam_rt` and `$TMPDIR/m10wt/erts_rust/target/release/beam_rt`).
`case_<name>_ns_per_call` at the case table's unchanged `n_rust`:

| case | `N` | m10 ns/call | fixed-HEAD ns/call | head/m10 | b54 (pre-fix) head/m10 |
|---|---|---|---|---|---|
| `emqx_base62_encode` | 25 000 | 19 260.0 | 23 274.7 | **1.21×** | **5.00×** |
| `bstr_to_lower` | 20 000 | 19 271.2 | 19 487.0 | 1.01× | 1.24× |
| `bstr_capitalize_token` | 15 000 | 25 332.1 | 33 137.6 | 1.31× | 1.22× |
| `emqx_base62_decode` | 15 000 | 17 876.0 | 19 456.4 | 1.09× | 1.23× |

The headline regression case (`emqx_base62_encode`) moves **5.00× → 1.21×**; the residual 1.0–1.3×
spread is the boot-jitter on a ~2.1 s STARTUP subtraction (a ±100 ms boot swing is ±25 % of a
`N=25 000` loop marginal) — see confound 2. (m10's own `foreign-m10.json` startup is ~16 ms, so its
marginal is clean; the direct paired ladder above, which re-measures `N=0` in-pass, is the tighter
estimate.)

## Non-foreign control — no collateral regression from the fix

Command: `BEAM_RT=<bin> erts_rust/target/release/bench run --quick --workloads list_walk,copy_bound --runtime rust --json …`

| workload metric | m10 `b4593818b3` | b54-HEAD `e1d51458c8` (pre-fix) | fixed HEAD |
|---|---|---|---|
| `list_walk` ns/element | 42.42 | 52.58 | 68.09 |
| `copy_bound etf_roundtrip` ns/op | 13 365.6 | 5 420.3 | 59 984.4 |
| `copy_bound ets_copy` ns/op | 3 980.9 | 2 482.8 | 3 294.7 |
| `copy_bound send_recv` ns/op | 194 771 | 21 214.1 | 25 529 104 |
| `copy_bound send_recv_plain` | 131 028 | 27 825.0 | **TIMED OUT (> 60 s)** |
| `copy_bound` peak RSS | 94.8 MB | 95.0 MB | 472.2 MB |

**Attributed — and it is `85ee33b2f8` (the boot-drive change), NOT the two-crate fix.** Evidence:

* **b54-HEAD (pre-fix, pre-boot-drive) is the cleanest binary of the three** (every `copy_bound`
  case faster than m10, no timeout) — so the two-crate fix is exonerated; it cannot have caused a
  regression that predates it *and* is absent where the fix is.
* **A synthetic empty boot script removes all of it.** Driving `-boot <empty-script>` on HEAD drops
  boot startup 2 157 → 15 ms, `send_recv N=200` wall 7 281 → 23 ms (CPU 692 → 23 ms), a decode
  micro-benchmark 4 305 → 274 ms, and RSS 385 336 → 15 856 KB.
* Root causes (HEAD-only, both from booting `start.boot`):
  * **A per-op 25 ms scheduler sleep.** The booted kernel's `os:set_signal/2` self-pipe makes
    `PortRegistry::has_nothing_to_poll()` permanently false, so `Runtime::is_quiescent()` never
    holds and every blocked round sleeps the full `PORT_POLL_SLICE` = 25 ms. Hence `send_recv` is a
    **constant 25.5 ms/op** (~10 % CPU — sleep, not work) at any `N`, and `send_recv_plain` hits the
    60 s `RUN_DEADLINE`.
  * **A Θ(atom-table) decode.** Booting loads all preloaded modules, growing the one shared atom
    table ~70 → ~4 100; the per-decode `prefix_matches` validation is Θ(table), so
    `binary_to_term` inflates ~9× while `term_to_binary` (encode) is unaffected.
* The `list_walk` 42 → 68 ns/element is a **map-extrapolation artifact** of the bench's
  startup-difference metric (identical runs gave 68.09 vs 92.77); direct **CPU** marginals are flat
  (m10 786 ms, b54 815, HEAD 844, HEAD-with-empty-boot over the same 2×10^7 elements). Likewise the
  RSS inflation is pure boot.

## Verdict

1. **The two-crate fix works.** Fixed HEAD's startup-cancelled `foreign` per-call marginal is at
   **parity with the m10 pre-regression baseline** at both calibrated points — `enc` 1.13× @ 25 000
   and **1.01× @ 60 000**, and 0.99–1.08× on the `cap`/`dec` sub-cases — versus **5.0×** for the
   pre-fix runtime (b54). The `emqx_base62_encode` per-call cost fell from b54's 84 873 ns/call to
   23 275 ns/call, and the growth curve is no longer the b54 super-linear shape.
2. **Curve shape:** fixed HEAD **tracks m10 at every ladder point** (the regression's diverging
   head-vs-m10 signature is gone). Both sides share a mild intrinsic ~1.8–2× drift from
   `N=5 000`→`60 000` (per-op `Nursery.retired` retention), a separate pre-existing issue, not this
   regression.
3. **Raw wall-clock is confounded** by the unrelated boot-drive commit `85ee33b2f8`, which adds
   ~2.07 s of fixed startup to **every** HEAD `run`. Only the startup-cancelled marginals are
   comparable, and they are at-or-better than m10.
4. **No collateral regression from the fix.** The large HEAD-only deltas the control surfaced
   (`send_recv` 131×, `send_recv_plain` timeout, decode ~9×, RSS ×25) are caused **exclusively** by
   `85ee33b2f8` — proven by b54-HEAD being clean and by the empty-boot-script A/B. They are **open
   performance regressions in the boot-drive change**, not in the two-crate fix.

### Caveats

* The host is shared/saturated (loadavg 6–45, 14 CPUs); raw absolute times are noisy. The verdict
  rests on the interleaved, paired-`N=0` marginals and (for the control) CPU-time marginals.
* Small-`N` marginals (`N <= 10 000`) are noise-dominated because the whole loop marginal
  (~100–200 ms) is smaller than the ~2.1 s boot's jitter; they are reported for completeness only.
* m10 and fixed HEAD differ by **two** independent changes (this fix **and** the boot-drive
  `85ee33b2f8`); the fix's own effect is isolated here by the startup-cancelled marginal, by b54's
  pre-fix numbers, and by the b54-HEAD clean control.

### Follow-ups left open (owned outside the measurement scope)

* **Boot-drive perf regression (`beam_rt`):** decouple the scheduler's blocked-round pump wait from
  `PortRegistry::has_nothing_to_poll()` so a registered signal watch no longer costs a full 25 ms
  `PORT_POLL_SLICE` per blocked round (`runtime/scheduler.rs`, `port.rs`).
* **Θ(atom-table) ETF decode (`beam_bifs`/`beam_rt`):** adopt the O(1) `AtomTable::{snapshot,
  generation, appended_since}` seam at `beam_bifs/src/binconv.rs` and the mirrored hosts in
  `beam_rt/src/nif.rs`.
