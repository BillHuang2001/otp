# b54 — `foreign` per-call growth curve (controlled experiment)

Raw evidence for the M11 `foreign` `ns_per_call` verdict: **the Rust per-call cost is
N-dependent (super-linear), in *both* execution tiers**, while the m10 baseline is
~(near-)linear over the same range. This is a plain measurement record — every value is
raw; nothing here is estimated. No metric inside any `results/*.json` artifact is altered
by this file.

* **HEAD** = `beam_rt` built from the commit this document is committed with
  (`e1d51458c8…`, the m12/BATCH-54 working tree), `--release`, default tier unless noted.
* **m10** = `beam_rt` built from `b4593818b3` (the m10 milestone commit), `--release`, in a
  **detached scratch worktree** (`git worktree add --detach $TMPDIR/m10wt b4593818b3`). The
  worktree path is a scratch path and is **never** baked into any committed artifact except
  as the literal `$TMPDIR/m10wt` placeholder in this file.
* Oracle for the foreign sources: system `erlc` (OTP 29.1.1 / erts-17.1).

## What is being measured

The `enc` case from `src/workloads/foreign.rs`: the driver function
`bench_foreign_driver:enc/1` loops `N` times calling the **real** foreign function
`emqx_base62:encode(<<1,2,3,4,5,6,7,8,9,10,11,12>>)`, accumulating `byte_size/1` each time
(result `= N × 16`). `cowboy_bstr.erl` and `emqx_base62.erl` are the unmodified checkout
sources (`/home/bill/Source/cowboy/src/cowboy_bstr.erl`,
`/home/bill/Source/emqx/apps/emqx/src/emqx_base62.erl`).

### Reproduction (exact commands)

```sh
# 1. Build both runtimes (from the repo root; the committed wrapper supplies CARGO_HOME).
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt

# 2. The m10 binary in a detached scratch worktree.
git worktree add --detach "$TMPDIR/m10wt" b4593818b3
( cd "$TMPDIR/m10wt" && ./erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt )

# 3. Stage + compile the driver together with the two real foreign sources (system erlc).
#    The driver source is the `DRIVER_SOURCE` constant in src/workloads/foreign.rs,
#    written verbatim to "$TMPDIR/b54exp/bench_foreign_driver.erl".
mkdir -p "$TMPDIR/b54exp"
cp /home/bill/Source/cowboy/src/cowboy_bstr.erl "$TMPDIR/b54exp/"
cp /home/bill/Source/emqx/apps/emqx/src/emqx_base62.erl "$TMPDIR/b54exp/"
# (write DRIVER_SOURCE → "$TMPDIR/b54exp/bench_foreign_driver.erl")
( cd "$TMPDIR/b54exp" && erlc -o "$TMPDIR/b54exp" cowboy_bstr.erl emqx_base62.erl bench_foreign_driver.erl )

# 4. The reproduction vehicle — one direct process per point, wall wall-clock via
#    `date +%s%N`, peak RSS via a /proc/<pid>/status VmHWM poll (~2 ms, as bench's
#    proc.rs does). `$BIN` is the HEAD or the $TMPDIR/m10wt binary; prepend
#    `BEAM_RT_TIER=interpreter` for the interpreter ladder.
"$BIN" -pa "$TMPDIR/b54exp" run "$TMPDIR/b54exp/bench_foreign_driver.beam" \
    bench_foreign_driver:enc/1 "$N"
```

The ladders are **interleaved per `N`** (m10 then HEAD at each `N`) so a drifting host load
affects both sides of every pair.

## Default tier (`BEAM_RT_TIER` unset ⇒ native baseline JIT), `fun=enc`

`/proc/loadavg` (1/5/15-min) **before** the pass: `16.87 23.05 20.72`
(46/1088 runnable/total; 6 CPUs online). Per-`N` loadavg recorded below; **after**: `15.79 22.34 20.56`.

| `N` | m10 wall_ms | HEAD wall_ms | m10 peak VmHWM (B) | HEAD peak VmHWM (B) | loadavg after N |
|---|---|---|---|---|---|
| 0 | 42 | 42 | 16 171 008 | 12 406 784 | 16.87 23.05 20.72 |
| 2 000 | 49 | 102 | 19 214 336 | 19 865 600 | 16.87 23.05 20.72 |
| 5 000 | 160 | 114 | 25 890 816 | 26 562 560 | 16.87 23.05 20.72 |
| 10 000 | 180 | 301 | 34 840 576 | 36 315 136 | 17.05 22.98 20.72 |
| 20 000 | 379 | 1 446 | 55 492 608 | 56 467 456 | 17.05 22.98 20.72 |
| 25 000 | 538 | 2 429 | 64 970 752 | 65 777 664 | 18.00 23.08 20.76 |
| 50 000 | 1 694 | 14 748 | 112 619 520 | 114 671 616 | 15.79 22.34 20.56 |

Head ← m10 wall ratios at matched `N`: 2 000 → 2.1×, 5 000 → 0.71× (noise-dominated: both
marginals are small beside the ~42 ms startup), 10 000 → 1.7×, 20 000 → **3.8×**,
25 000 → **4.5×**, 50 000 → **8.7×**. The ratio grows monotonically with `N` once the loop
marginal clears startup — the signature of a per-call cost that itself scales with `N`.

Derived startup-cancelled marginals `(wall(N) − wall(0)) / N`, in µs/call (derived from the
raw walls above, single sample each; the small-`N` m10 cells are load noise, not a trend):

| `N` | m10 µs/call | HEAD µs/call |
|---|---|---|
| 2 000 | 3.5 | 30.0 |
| 5 000 | 23.6 | 14.4 |
| 10 000 | 13.8 | 25.9 |
| 20 000 | 16.9 | **70.2** |
| 25 000 | 19.8 | **95.5** |
| 50 000 | 33.0 | **294.1** |

HEAD's per-call figure rises ~10× from N=2 000 to N=50 000 (25× the loops); m10's stays in
the 3.5–33 µs band. Peak RSS grows ~**2 KiB/iteration** on *both* binaries
(m10: (112 619 520 − 16 171 008)/50 000 = 1 929 B/iter; HEAD: 2 045 B/iter) — the retention is
**not** what distinguishes the two.

## Interpreter tier (`BEAM_RT_TIER=interpreter`), `fun=enc`

`/proc/loadavg` before: `15.79 22.34 20.56` (3/907 runnable/total). **After**: `15.79 22.34 20.56`
(unchanged through the whole pass).

| `N` | m10 wall_ms | HEAD wall_ms | m10 peak VmHWM (B) | HEAD peak VmHWM (B) |
|---|---|---|---|---|
| 0 | 23 | 22 | 15 708 160 | 12 431 360 |
| 500 | 41 | 33 | 17 842 176 | 18 006 016 |
| 1 000 | 51 | 46 | 18 239 488 | 19 025 920 |
| 2 000 | 58 | 107 | 21 213 184 | 21 147 648 |
| 4 000 | 84 | 466 | 25 014 272 | 25 038 848 |

Derived µs/call: m10 36.0 / 28.0 / 17.5 / 15.3 at N = 500 / 1 000 / 2 000 / 4 000 (flat);
HEAD 22.0 / 24.0 / **42.5** / **109.3** (rising). The ladder is deliberately capped at
`N = 4 000` because the interpreter tier is pathological above it (HEAD at `N` ≫ 4 000 is
minutes per run).

## Verdict this record supports

1. **The driver's own `--quick` run reproduces the effect at the case table's calibrated
   `N`** — see `results/b54-foreign-ab-{m10,head}.json`: at *matched* `N` (n_rust unchanged
   since m10: 20 000 / 15 000 / 25 000 / 15 000) HEAD vs m10 is
   `emqx_base62_encode` 84 873.04 vs 16 964.14 ns/call (**5.00×**),
   `bstr_to_lower` 24 240.25 vs 19 546.41 (1.24×),
   `bstr_capitalize_token` 28 326.86 vs 23 289.89 (1.22×),
   `emqx_base62_decode` 21 046.50 vs 17 147.75 (1.23×).
2. **Both execution tiers are super-linear at HEAD**, so the effect is **not** in the
   baseline-JIT emitter — it is shared by the interpreter, i.e. it lives in the shared
   per-process/slice machinery (a per-slice registration loop), not in code emission.
3. **The m10 baseline is ~linear in the same window**, so the m10→m11 move is a genuine
   regression, not a formula/`N`-mismatch artifact. The case table's `n_rust` is unchanged,
   so the m11 A/B was taken at matched `N`.
4. **Peak-RSS retention is invariant between the two binaries** (~2 KiB/iteration each) —
   the growth curve's memory column is *not* what explains the timing divergence.

The failing mechanism is owned outside this node (`beam_rt` / `beam_process`); this file
records the experiment, not the fix. See `RESULTS.md` §3.4 / §8 item 1.
