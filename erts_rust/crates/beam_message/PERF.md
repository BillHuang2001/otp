# `beam_message` — Measured: the per-message copy and messaging cost

The measurement record for this crate's share of the process copy boundary: one `send` — `copy_struct` + the
destination off-heap drain + the `Message` build + `Queue::enqueue` — for the shapes
`tests/{traversals,counters,send_perf,perf}.rs` build, plus the `copy_bound` bench A/B against the real ERTS
oracle (§7).
The **primary signal is deterministic and structural** (§2 traversals, §3 counters): it is profile- and
load-independent and reproduces exactly.
Every wall-clock figure (§4 release, §5 dev, §7–§9 A/Bs) is a ratio on a **labelled** regime, never an absolute
to quote.
Every assertion in these harnesses is correctness-only — the enqueued payload is `compare::eq` the original, exactly
one message is enqueued, and `dst.used()` advances by exactly the copier's own price; **no test asserts a timing
threshold, a walk count or an allocation count**.

## 1. Regimes, host and method

**Profiles — never mix them.**

| regime | what it is | sections |
|---|---|---|
| **dev** | plain `cargo test` — crate `opt-level = 1`, deps `opt-level = 2` | §5 |
| **release** | `--release` — `opt-level = 3`, `lto = "thin"`, `codegen-units = 1` (the profile the crate ships) | §4, §6–§9 |

**Host:** shared 14-core sandbox, no isolation; cpu0–3 are 4.7 GHz P-cores and cpu4–15 are 3.3–3.7 GHz E-cores, so an
unpinned run lands on either core type.
Every release figure here is `taskset -c 3`.
`/proc/loadavg` is recorded before and after every run; the host is *heavily* shared, so the load is part of the
record, not noise to hide. Three sessions are quoted and must not be mixed:
`m17` (the bench record this crate previously cited, 1-min load 3.3–92.6), and **this pass** which took
(a) the structural traversals run at load **50.1**, (b) the both-runtimes bench pass at **103.4 → 96.0**, and
(c) the old-vs-new runtime A/B at **83.7 → 37.6** and **67.6 → 39.7** (pinned, alternating order).

**Method.**

* `tests/traversals.rs` — the **structural traversal counter** (§2). Five shapes, one `send` each inside an
  allocation window: a counting `#[global_allocator]` (blocks, bytes, an 8-byte size histogram), the destination's
  `used()` advance or the fragment's, the copier's own **write census**, and the `size_object`-style **graph census**
  of the same term. `REPEATS = 8` windows per row, each asserted **identical**, and the histogram is asserted to sum
  to the block count. Plus the literal-sharing itemisation (`1 object / 3 words` written vs `28 / 73` censused) and
  its `clear_literal` control.
* `tests/counters.rs` — the per-message census + allocation counters of the **fits** path (§3), three counted phases
  (bare `copy_struct`; `send` into a cold queue; `send` into a warm queue), `REPEATS = 8`, all identical.
* `tests/perf.rs` / `tests/send_perf.rs` — the dependency-free wall-clock micro-benchmarks of the copy and of a whole
  `send` (§4, §5), and the single-pass-vs-sized probe.
* `tests/identity.rs` — the copy-boundary identity evidence (`=:=`, byte-identical `term_to_binary`, copy-of-copy,
  and `binary_to_term` of the copy's own image); correctness only, no figure.
* `erts_rust/tools/bench` (§7) — the external `copy_bound` workload under **both** runtimes, with the driver's own
  **paired** estimator: each round samples the `N`-op pass and the `0`-op pass back to back and takes the median
  paired difference, so a drifting boot cost cancels inside the pair; `N` is per runtime and every row carries the
  deterministic `case_<name>_ops` companion. Both sides are recorded by the driver (`sys` = the `erl` on `PATH`,
  OTP 29.1.1/erts-17.1; `rust` = the release `beam_rt`).

Timing is `std::time::Instant` (no `criterion`, no new dependency); a mean over an iteration batch is load-sensitive,
so `best`/`min` is always reported beside it.

## 2. Structural traversals per send shape (the primary signal)

The payload is the exact `copy_bound` 5-key map (**70 words / 27 objects / 21 cons cells + 6 headed**), sent as the
2-tuple `{From, Payload}` — so the graph **census** (`size_object`-style, descending into a literal area) is
**28 objects / 73 words**, while the **copier's** price for the literal-tagged twin is **1 object / 3 words** (the
outer tuple only: a literal-area subterm is shared by identity). Reproduction:
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message --test traversals -- --nocapture`.

**The "after" table** (this pass, `/proc/loadavg` 50.1 → 50.1, dev profile, identical across all 8 windows/row):

```
shape                       src_objs src_words   w_objs  w_words residency   blocks       bytes top-4 alloc sizes               ns_min
literal {From,Lit} (fits)         28       73        1        3   on-heap        1         480 480-487B:1                         434
heap twin {From,Map} (fits)       28       73       28       73   on-heap        3         672 64-71B:1 128-135B:1 480-487B:1       773
heap twin, 1 word short           28       73        0        0   refused       10        1468 64-71B:2 128-135B:2 16-23B:1 48-55B:1      3047
heap twin, full heap (frag)       28       73       28       73  fragment       13        2588 64-71B:2 128-135B:2 16-23B:1 32-39B:1     10310
ets_copy 233-w nursery            28       73        0        0   refused       10        1468 64-71B:2 128-135B:2 16-23B:1 48-55B:1      2849
```

**Before → after, per row** (the "before" side is the same instrument on the pre-collapse commit):

| row | residency | written words/objects | blocks | bytes |
|---|---|---|---|---|
| `literal {From,Lit}` (fits) | on-heap | 3 / 1 → 3 / 1 (unchanged) | 1 → **1** | 480 → **480** |
| `heap twin {From,Map}` (fits) | on-heap | 73 / 28 → 73 / 28 (unchanged) | 3 → **3** | 672 → **672** |
| `heap twin, 1 word short` | refused | 0 / 0 → 0 / 0 | 30 → **10** (−66.7 %) | 4 624 → **1 468** (−68.2 %) |
| `heap twin, full heap (frag)` | fragment | 73 / 28 → 73 / 28 | 35 → **13** (−62.9 %) | 5 812 → **2 588** (−55.5 %) |
| `ets_copy 233-w nursery` | refused | 0 / 0 → 0 / 0 | 30 → **10** (−66.7 %) | 4 624 → **1 468** (−68.2 %) |

**Reading:**

* **The two `fits` rows are unchanged to the byte** — exactly what the collapse predicts, because they never reach
  the pricing walk: they are one copy traversal (`try_copy_fast`) and no preflight. Their blocks are the copy's
  fix-up `Vec<Fixup>` ladder (64 B, 128 B) plus the enqueued message's queue buffer (480 B, once per mailbox); the
  literal row writes no child, so it has no fix-up allocation at all.
* **The three refusal/fragment rows are where the collapse shows**: **10 blocks / 1 468 B** for a refusal against
  the retired composition's 30 / 4 624 (−67 % / −68 %), and 13 / 2 588 for the fragment path against 35 / 5 812
  (−63 % / −55 %). This is the whole of the change: the same refusal now costs **one** pricing walk instead of the
  retired bounded-walk → `is_acyclic` → `size_object_with` chain, and the fragment is sized from that walk's own
  exact figure instead of a second `size_object`.
* The 10 remaining blocks of a refusal decompose by size bucket, and every one is accounted for: the copy's trial
  fix-up `Vec<Fixup>` (64 B, 128 B), `beam_term`'s **per-walk `Vec<Step>`** ladder (16 B → 512 B) and that walk's
  SipHash **`HashSet` path-set** ladder (48 B, 88 B, 160 B). The fragment row adds the fragment's own backing region
  (608 B = the 73-word payload plus the fragment header) and the queue buffer (480 B); the refusal rows enqueue
  nothing, so they pay neither.
* Deterministic, not timing: every counter window is asserted **identical** across `REPEATS = 8` runs and the
  histogram is asserted to sum to the block count, so a stray concurrent allocation fails the run instead of
  inflating a published figure.

**Traversals per send shape, as the crate now performs it:**

| shape | copy trails | pricing trails | what happens |
|---|---|---|---|
| **fits**, destination off-heap table empty (the common `send`, a fresh staging table) | **1** | **0** | `try_copy_fast` writes the whole graph into the destination's unused tail and commits with one `alloc_words`. No preflight at all. |
| fits, destination off-heap table **non-empty** | 1 | **1** | the pricing walk sizes it, then the ordinary write loop runs (`tests/destination_pricing.rs` pins both routes). |
| **refused** (destination too small) | 1 partial trial + 1 (the caller's retry, below) | **1** (exact price) | the optimistic trial is abandoned (nothing written: `used() == peak_used()`), **one** pricing walk yields the exact figure, the typed `Alloc(Message)` refusal carries it, and no word is copied. |
| **refused, then fragmented** (`send_fragmented`) | the above **+ 1** copy into the fragment | **1** | the fragment is sized from the refusal's **own** figure (`AllocFailure::requested / size_of::<Eterm>()`), so nothing is re-copied and no second sizing walk runs; `fragment_capacity_words` is a floor, not the price. |

A whole `beam_rt` delivery still attempts once in `Process::deliver` before `beam_message` is asked again, so a
*refused delivery* is **2 copy attempts + 1 pricing walk** by contract — the runtime's pre-attempt, the collapse, and
the runtime's `grow_nursery` retry (or the fragment fallback). That first attempt is the residual owner's to remove
(§10).

## 3. Deterministic counters per message (the fits path)

Per-message work for a flat copy into a clean `words + 64`-word heap — `tests/counters.rs`, identical in dev and
release builds:

| shape | words | objects | cons cells | headed | header words | header % | words/object | bytes touched |
|---|---|---|---|---|---|---|---|---|
| small tuple | 4 | 1 | 0 | 1 | 1 | 25.0 % | 4.00 | 64 |
| 10-int list | 20 | 10 | 10 | 0 | 0 | 0.0 % | 2.00 | 320 |
| HAMT map (64 keys) | 234 | 85 | 64 | 21 | 21 | 9.0 % | 2.75 | 3 744 |
| **`send_recv` payload** | **70** | **27** | **21** | **6** | **6** | **8.6 %** | **2.59** | **1 120** |

`words` is `size_of_copy(term)` (= `size_object`), cross-checked against the destination's `used()` advance.
`bytes touched = 2 · words · 8 B` (each source word read once, each destination word written once); its precondition is
asserted — every destination starts with a **fully empty** off-heap table, which is exactly
`copy.rs::is_fast_path_eligible`, so the copier takes its single-pass path and never reads the graph twice.
A destination whose off-heap table is non-empty is the one case this figure would understate.

Allocations per message — the counting `#[global_allocator]`, `allocs / bytes`:

| shape | `copy_struct` | `send` (warm queue) | enqueue half = warm send − copy | `send` (cold queue) |
|---|---|---|---|---|
| small tuple | 0 / 0 B | 0 / 0 B | **0 / 0 B** | 1 / 480 B |
| 10-int list | 1 / 64 B | 1 / 64 B | **0 / 0 B** | 2 / 544 B |
| HAMT map (64 keys) | 4 / 960 B | 4 / 960 B | **0 / 0 B** | 5 / 1 440 B |
| **`send_recv` payload** | **2 / 192 B** | **2 / 192 B** | **0 / 0 B** | 3 / 672 B |

**Reading:**

* **The enqueue half allocates nothing**: warm `send` minus the bare copy is `0` allocations / `0` bytes for every
  shape, so the `Message` build and the FIFO push are allocation-free.
  The cold-queue `send` is exactly one allocation larger than the copy (480 B, identical for every shape) — that is the
  queue's **first buffer**, paid once per mailbox, never per message.
* **The copy's allocations are the fix-up worklist `Vec<Fixup>` and nothing else** (§8). A `Fixup` is 16 B
  (`(*mut Eterm, Eterm)`), so the event counts are the `Vec`'s capacity doublings: 0 for the small tuple (no non-shared
  children), 1 × 64 B (cap 4) for the 10-int list, 2 × 64 + 128 B = **192 B (cap 4 → 8) for the `send_recv` payload**,
  4 × 64 + 128 + 256 + 512 B = 960 B (cap 4 → 32) for the 64-key HAMT.
* The payload's per-message cost is therefore **2 blocks / 192 B, all of it the worklist**; it moves only if the worklist
  does, not with load, profile or shape-independent tuning.
* The 1 120 B touched for the payload is 70 words: 42 cons-cell words (21 × 2, header-less) + 22 headed payload words +
  6 header words; header overhead is 8.6 %, max object 8 words.

## 4. Wall clock — release (the shipping profile)

`send_perf`, `taskset -c 3`, 5 runs at `/proc/loadavg` 29.5–36.4; per shape the **median of the 5 runs' means** and the
**best** single send seen (the two spiked small-tuple entries, 165.0 and 63.4 ns mean, came from the runs at load 34.6 —
the load's, not the code's):

| shape (clean destination) | words | objects | median mean ns/send | best ns/send | ns/word |
|---|---|---|---|---|---|
| small tuple | 4 | 1 | 51.9 | 42 | 13.0 |
| 10-int list | 20 | 10 | 85.1 | 74 | 4.3 |
| 8 × 32-byte binaries | 72 | 16 | 194.9 | 178 | 2.7 |
| HAMT map (64 keys) | 234 | 85 | 559.3 | 477 | 2.4 |
| **`send_recv` payload** | **70** | **27** | **198.8** | **176** | **2.8** |

`objects` is the §3 census where the shape is in `counters`; for the binaries shape it is the structural count
(8 cons + 8 binary objects). `send_perf`'s own printed column carries a hard-coded structural estimate (65 for the HAMT,
26 for the payload) that the census supersedes.

* **The whole `send` for the `send_recv` payload is ≈ 199 ns mean, 176 ns best** — 5 runs spanned 196.4–205.5 ns mean
  and 176–179 ns best, so **quote ≈ 200 ns**, with that run-to-run band as its uncertainty.
  The §9 A/B's 210.4 ns mean / 181 ns best is the same figure from a different session at a lower load.
* Clean and dirty destinations are the **same path** and the same cost: `dirty − clean` stays within ±3 % on every
  70-word-and-up shape and changes sign run to run, and only the 4-word tuple swings wider (−57…−0.4 ns, on a ≈ 50 ns
  absolute cost). That is noise, because the single-pass gate is the off-heap table's emptiness, not the heap's
  `used()`.
* The copy-vs-enqueue split (`counters` under `--release`, one run at `taskset -c 3`, loadavg 28.6): `copy_struct`
  **167.2 ns mean / 149 best**, whole `send` **190.7 / 173**, enqueue phase **23.5 ns** — i.e. ≈ 88 % of a `send` is the
  copy traversal itself and ≈ 12 % the `Message` build + enqueue.
* Scaling between shapes is what the work is: 13.0 ns/word for the fixed-cost-dominated 4-word tuple falls to 2.4–2.8
  ns/word for the 70–234-word graphs.

## 5. Wall clock — dev profile (`perf` and `send_perf` under `cargo test`)

A dev-profile absolute is **not** a shipping figure: the same binary's payload `send` read 290 / 339 / 399 / 424 ns
mean across 4 runs of the same build at loadavg 8.1–16.7. Use these for their **ratios and stability only**.

`tests/perf.rs`, `copy_struct` alone, 4 runs at loadavg 8.1–16.7 (`best_ns` is the least load-sensitive column; the
`mean` over the runs in parentheses):

| shape | words | best ns/copy (range) | mean ns/copy (range) | ns/word |
|---|---|---|---|---|
| small tuple | 4 | 39–40 | 41.6–42.3 (one 135.7 spike) | 10.4 |
| deep list (2000 cons cells) | 4 000 | 7 417–7 447 | 8 521–8 902 (one 11 988 spike) | 2.1–2.2 |
| flat map (8 keys) | 20 | 46–48 | 49.7–53.8 | 2.6 |
| HAMT map (64 keys) | 234 | 648–673 | 700.7–750.7 | 3.0 |
| refc binary reuse (512 B) | 8 | — | 36.1–42.3 | 4.5–5.3 (payload shared, not deep-copied) |

`tests/send_perf.rs`, per shape the range over 4 runs (clean destination; the dirty row tracks it):

| shape | words | mean ns/send (range) | best ns/send (range) |
|---|---|---|---|
| small tuple | 4 | 80.7–107.4 | 51–87 |
| 10-int list | 20 | 112.1–169.9 | 90–149 |
| 8 × 32-byte binaries | 72 | 272.1–350.9 | 225–312 |
| HAMT map (64 keys) | 234 | 770.8–1 274.5 | 697–1 222 |
| `send_recv` payload | 70 | 289.6–424.2 | 244–367 |

**The sized path's pricing walk** (`tests/perf.rs`, `[single-pass]` = empty destination off-heap table vs
`[sized]` = a non-empty one, which forces pricing-then-writing): the sized row costs more in every run but one, whose
negative reading (−21 ns at loadavg 16.7) marks the measurement's floor.
On `best_ns` the small tuple is 51–52 ns sized against 38–40 ns single-pass (+12–14 ns, ≈ +30 %); the flat map is
77–119 ns sized against a 76 ns single-pass, so its share is at or below this host's noise in dev.
The share is **directional evidence, not a pinned percentage** — quote it only with its profile.
The refusal path is priced deterministically instead (§2): **one** `beam_term` walk whose cost is that crate's
`Vec<Step>` + `HashSet` (§10).

## 6. This crate's share of `send_recv_plain`

Current record — this pass, `copy_bound`, both sides, the driver's paired estimator, `--full` (10 paired rounds/case),
`taskset -c 3`, `/proc/loadavg` 103.4 → 96.0:

| case (rust / sys) | sys ns/op | rust ns/op | ratio |
|---|---|---|---|
| `etf_roundtrip` | 801.0 | 2 051.9 | 2.56× |
| `ets_copy` | 324.6 | 2 700.7 | 8.32× |
| `send_recv` | 1 834.8 | 8 154.0 | 4.44× |
| `send_recv_plain` | 805.8 | 3 058.9 | 3.80× |

The previous record (m17, 6 paired rounds/case, `../tools/bench/RESULTS.md` §3.3) read `etf_roundtrip` 2.21×,
`ets_copy` 9.28×, `send_recv` 5.39×, `send_recv_plain` 3.16×. **The two sessions are not comparable in absolute terms**
(m17's 1-min load band was 3.3–92.6, this pass's was 96–105, and the sys side alone moved 279.3 → 324.6 ns on
`ets_copy`); every ratio above is paired within its own session, and §7 is the only "changed since" evidence.

**The workload performs two payload sends per op, not one** — `send_recv_plain` sends `Worker ! {self(), T}` and the
worker replies `From ! {self(), T}` — and each send is a 2-tuple (3 words) around the 70-word payload.
At the measured release figures (§4: 198.8 ns for the payload; the 4-word tuple at 51.9 ns gives ≈ 2.2 ns/word of
marginal cost, so a 73-word send is ≈ 206 ns) the two sends are **≈ 0.41 µs/op**:

> **≈ 13–19 % of a per-op figure is `beam_message`'s `send`** — 0.41 µs against this pass's 3.06 µs (13 %) and against
> m17's 2.20 µs (19 %) — and this is a **lower bound**: `beam_process::deliver` (message registration, receiver wake)
> and `beam_rt`'s scheduling, receive scan and pattern match are outside the measured `send`.

So even a **free** copy would leave the workload at ≈ 2.5–2.7 µs/op. The residual is owned by:

* **`beam_rt` — the per-slice runner work.** `../beam_rt/CONTEXT.md` and `src/runner/CONTEXT.md` *Measured* enumerate it:
  the engine moving out of/into a `HashMap` per slice, a fresh `RuntimeContext` + `DispatchMemo` per slice,
  `ctx.enter_module`'s `String`, the park/resume root publish, the receive-timeout timer heap and the per-boundary wake
  sweep. No part of it is a pass sleep: the run route's pass wait is progress-aware and issues **0**
  `clock_nanosleep` on a `send_recv`-shaped run, and `PORT_POLL_SLICE` is a non-destructive zero-timeout probe.
* **`beam_process` — the delivery path.** `Process::copy_message` (the receiver's long-lived, dirty young heap that the
  single-pass gate exists for), `Process::deliver`/`deliver_message`/`run_rooted_collection`, and the mailbox +
  `QueueCursor` selective-receive scan.
* **`beam_codec` — for the `send_recv` case only** (the `term_to_binary`/`binary_to_term` round trip the plain case
  omits). **The `send_recv` vs `send_recv_plain` delta is that round trip, not the copy** — the plain case's copies are
  3 words each (the literal is shared, §2), so a copy-bound attribution of that pair is a category error until someone
  measures the codec's share directly.

**Handoff:** attribution of the remaining ≈ 2.5 µs must come from **counters inside `beam_rt`'s runner** (slices, parks,
wakes, allocations per op), not from this file; `tests/send_perf.rs` is the micro-benchmark that measures this crate's
half of it and is `taskset -c 3`-pinnable.

## 7. Bench A/B — `copy_bound`, the two runtimes and the two binaries

Reproduction (never write into the tracked `tools/bench/results/`; `--out` a `$TMPDIR` directory):

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --release
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p bench   --release
taskset -c 3 erts_rust/target/release/bench run --full --workloads copy_bound --runtime all \
    --out "$TMPDIR/bench-after-full"
```

### 7.1 Cross-runtime (the recorded comparison, both sides by the driver)

`--full`, 10 paired rounds per case, `taskset -c 3`, `/proc/loadavg` **103.4 → 96.0**; `N` is the driver's per-runtime
count (`etf_roundtrip` 500 000/1.5 M, `ets_copy` 250 000/2 M, `send_recv` 90 000/0.5 M, `send_recv_plain`
300 000/1 M, rust/sys), emitted as `case_*_ops` in the report:

| case | sys ns/op | rust ns/op | rust/sys | driver's paired median (rust) | noise band | rounds |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | 801.0 | 2 051.9 | **2.56×** | 1 024.2 ms | ±69.4 ms | 10 |
| `ets_copy` | 324.6 | 2 700.7 | **8.32×** | 709.5 ms | ±345.5 ms (a withheld figure — see below) | 10 |
| `send_recv` | 1 834.8 | 8 154.0 | **4.44×** | 1 150.6 ms | ±373.9 ms | 10 |
| `send_recv_plain` | 805.8 | 3 058.9 | **3.80×** | 1 169.2 ms | ±139.3 ms | 10 |

One row was **withheld by the driver** on the rust side in the earlier `--quick` pass of the same session
(`ets_copy` → `marginal below timer resolution`, paired median 709.5 ms against a ±345.5 ms band) because the host was
at load 105; the `--full` pass above produced the figure. Read the ratios, never the absolutes: at this load the sys
side alone is inflated ~16 % against m17.

### 7.2 The runtime A/B (the only "changed since" evidence)

Two release `beam_rt` binaries from the **identical** tree except this crate's sources — `old` =
the pre-collapse commit, `new` = HEAD — driven by the same `bench` binary via `$BEAM_RT`, `--full`, `--runtime rust`,
`taskset -c 3`, **7 alternating passes** (5 in one set, order `old,new,old,new,old,new`; 2 in a second set with the
order **reversed**, `new,old`), loads **83.7 → 37.6** and **67.6 → 39.7**:

| case | old ns/op (median of 5) | new ns/op (median of 5) | ratio new/old | per-round paired ratios |
|---|---|---|---|---|
| `etf_roundtrip` | 1 895.6 | 1 960.4 | 1.03× | 0.975 1.018 1.04 1.22 (one old round withheld) |
| `ets_copy` | 2 560.4 | 2 563.0 | **1.00×** (no detectable change) | 0.993 1.004 1.001 1.019 1.218 |
| `send_recv` | 10 269.3 | 8 091.3 | **0.79×** (−21 %) | 0.817 0.723 0.792 0.788 0.759 |
| `send_recv_plain` | 2 565.0 | 3 152.2 | 1.23× (+23 %) | 1.34 1.208 1.209 1.223 1.207 |

The driver's own noise bands on these passes were ±5–56 ms on 700–1 100 ms medians (2–6 %), so the `send_recv` and
`send_recv_plain` separations are **outside** the band and reproduce in both orderings; `ets_copy`/`etf_roundtrip` are
inside it.

**Whole-run instruction and cycle counts (`perf stat`, `--quick --runtime rust --workloads copy_bound`,
`taskset -c 3`, one old/new/old/new set at load 29–71):**

| binary | `cpu_core/instructions/u` | `cpu_core/cycles/u` | task-clock |
|---|---|---|---|
| old | 319 194 037 273 / 319 675 490 376 | 76 840 725 051 / 76 838 845 995 | 25 338 / 24 986 ms |
| new | 314 055 294 111 / 314 388 228 736 | 76 794 030 932 / 76 800 604 910 | 24 960 / 24 864 ms |

**The new binary retires ~1.5 % *fewer* instructions and burns the same cycles over the whole run**, while the same
runs reproduce the per-case split (`send_recv` 10 418/9 856 → 7 866/7 868; `send_recv_plain` 2 573/2 507 →
3 022/3 103 ns/op). So the collapse did not add work anywhere; the `send_recv_plain` move is a redistribution inside
the case mix (`send_recv_plain`'s copies are 3 words, so no copy-path change can explain it) whose mechanism is **not
attributed here** — candidates are code placement in the thin-LTO binary and the changed fragment capacity the
runtime's delivery mix now allocates (this crate sizes a fragment from the copier's exact price, so a literal payload's
fragment went 73 words → 3). Attributing it needs instruction-level counters **inside `beam_rt`'s loop**, which is that
node's to add; until then `send_recv_plain` must not be quoted as either an improvement or a regression, and note that
the same case also reads 2 195.9 ns in m17 — i.e. it moves ±20 % across builds and sessions with no matching change in
work, which makes it the least claim-worthy of the four rows.

## 8. The fix-up worklist is a `Vec<Fixup>` — and that is the right call in release

The copy loop's per-call worklist is `Vec<Fixup>` (`src/copy.rs`), 16 B per entry, and §3 shows it is the **only**
per-message allocation: 0 / 1 / 2 / 4 allocation events for the four shapes.

An allocation-free replacement (a `WorkStack`: `[MaybeUninit<Fixup>; WORK_INLINE]` inline plus a `Vec` spill) removes
those events deterministically (counters: **0** per `copy_struct` on every shape but the 64-key HAMT) and does **not**
pay for itself.
Measured in **release** (`opt-level = 3`, `lto = "thin"`, `taskset -c 3`, 6 interleaved pairs, `/proc/loadavg` 17–23,
`WORK_INLINE = 8`): the `send_recv` payload was **neutral** (best-of-20 000 **187 ns vs 186 ns**) and the 10-int list was
**17 % slower** (best 90 ns vs 76 ns).
In **dev** (`opt-level = 1`) the regression is larger and visible on every list-shaped copy (deep list 2000 cons cells
+25 %, 64-key HAMT +11 %) and is a codegen effect of the inlined `pop` round-tripping the fix-up through a stack slot,
not frame size or `MaybeUninit` initialisation (both variants measured).
The `Vec<Fixup>` therefore ships: it is non-regressive in the shipping profile, and the allocation is not where the
per-send cost is.

## 9. The shared-term test runs once per copied object, not twice

A fix-up is only ever recorded for a child already proven **not** shared, so the worklist's pop side calls
`shallow_unshared` while the two root calls keep `shallow`'s `is_shared` guard — one guard (four tag tests) less per
copied object, no added state, no allocation change.

Measured in **release** (`opt-level = 3`, `lto = "thin"`, `taskset -c 3`, alternating sides, `/proc/loadavg` 17–24,
median of the per-run means, `best` = median of the runs' best-of-20 000, ns/send):

| shape (clean destination) | guard on both sides | guard once per object | Δ mean | Δ best |
|---|---|---|---|---|
| small tuple (4 words) | 57.4 / 39 | 55.0 / 37 | −4.2 % | −5.1 % |
| 10-int list (20 words) | 89.9 / 71 | 90.7 / 72 | +0.9 % (noise) | — |
| 8 × 32-byte binaries (72 words) | 250.0 / 178 | 228.6 / 170 | −8.6 % | −4.5 % |
| HAMT map, 64 keys (234 words) | 714.5 / 605 | 658.7 / 579 | −7.8 % | −4.3 % |
| `send_recv` payload (70 words) | 234.5 / 188 | 210.4 / 181 | −10.3 % | −3.7 % |

The saving scales with the number of copied objects, which is exactly the per-object guard it removes; the 10-int list,
one guard per cell on a 2-word object, is flat.
Deterministic counters are unaffected (the payload is still 2 blocks / 192 B in the copy, 0 in the enqueue half), and the
current measurement of the payload `send` (§4: ≈ 199 ns mean / 176 ns best) sits at the 210/181 column's cost.

## 10. What remains (by owner, after the collapse)

This crate's copy path is **one walk** on every shape it performs: no preflight for a fit, one exact pricing walk for a
refusal, and the fragment sized from that same figure. Every remaining cost is somebody else's seam:

* **`beam_term` (`src/size.rs`) — the pricing walk's own allocations.** The walk is `sizing_walk` (`:386`) with a
  `Vec<Step>` (`:389`) and a SipHash `HashSet<usize>` path set (`:388`); those two ladders are 8 of the 10 blocks a
  refusal costs (§2) and they recur per walk. Two adoptable shapes, in `## Handoffs` of `CONTEXT.md`: a
  **bounded-with** walk that latches "over limit" instead of returning early, so one walk yields both the exact price
  and the fits/doesn't verdict (today a caller that wants *both* must pay `is_acyclic`'s two more hash tables); and a
  bounded fixed-stack cycle check in place of the path set (the legacy C has no cycle check at all — it trusts acyclic
  heaps).
* **`beam_rt` (`src/context/calls.rs`, `src/runner/`) — the pre-attempt + `grow_nursery` retry.** A refused *delivery*
  is 2 copy attempts + 1 pricing walk by contract (§2): the runtime attempts in `Process::deliver`, then retries. The
  message-path analogue of the existing `copy_out_hint`/`hint_grow_gate` gate (`calls.rs:1313-1332`) is what would
  stop the double attempt. That node's prose still describes the retired 3-walk/5-traversal path (`calls.rs:1280`,
  `:1316`, `:1389`, `context/runtime_context.rs:159`, `context/calls/tests.rs:190`, `:529`, `PERF_DISPATCH.md:252`,
  `:279`, `:387`, `crates/beam_engine/src/bench/CONTEXT.md:242`, `:270`).
* **`beam_process` (`src/ptab.rs:284`) — control-signal fragments.** `HeapFragment::new(size_object(payload) + 8)`
  walks the graph once just to size the fragment; adopting this crate's refusal-derived exact price
  (`AllocFailure::requested / size_of::<Eterm>()`) removes that walk, exactly as `send_fragmented` already does.
* **`beam_codec` — `send_recv` vs `send_recv_plain`.** The delta between those two cases (§6) is the worker's
  `binary_to_term(term_to_binary(T))` round trip; the plain case's copies are 3 words, so the copy is not part of it.
  Measure the codec's share before claiming any.
* **The runtime's own per-op cost is the whole story for `send_recv_plain`** (≈ 3.06 µs/op against this crate's
  ≈ 0.41 µs of send, §6), and §7.2 records the one unattributed move in it.

## 11. Reproduce

```sh
# the structural traversal table (§2) — the primary signal
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
    --test traversals -- --nocapture
# the per-message counters (§3)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
    --test counters -- --nocapture
# the copy-boundary identity evidence (correctness, no figure)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
    --test identity -- --nocapture
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
    --test perf -- --nocapture
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
    --test send_perf -- --nocapture

# the release regime (the one to quote for a per-send shape):
erts_rust/scripts/cargo.sh build -p beam_message --manifest-path erts_rust/Cargo.toml --release --tests
taskset -c 3 erts_rust/target/release/deps/send_perf-* --nocapture
taskset -c 3 erts_rust/target/release/deps/counters-*  --nocapture
```

Always go through `erts_rust/scripts/cargo.sh` (`$HOME` is read-only) and pass `--manifest-path erts_rust/Cargo.toml`;
never `cd`.
Record `/proc/loadavg` before and after every run and quote it with the figure.
For the bench A/B (§7) always `--out "$TMPDIR/…"` — the tracked `../tools/bench/results/` is the sibling's milestone
record, and an old-vs-new side is driven by `$BEAM_RT=<path> bench run --runtime rust`, alternating the two binaries
round by round and reversing the order in a second set (the order is not a variable this host lets you ignore).
This crate's tests are **all self-checks** — no test in `src/` or `tests/` invokes `erl`/`erlc`; the oracle
cross-checks live in the sibling `../tools/differential/` harness.
