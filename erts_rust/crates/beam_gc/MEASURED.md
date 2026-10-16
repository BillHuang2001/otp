# `beam_gc` — Measured: Copying vs Immix across mature-generation workloads

The measurement artifact for the two mature-generation strategies of this crate — the default
**copying** mature generation (`Collector`) and the **mark-region** one (`GenImmixCollector`),
both reached through `AnyCollector` / `GcConfig`.

The crate `CONTEXT.md` carries only the load-bearing conclusions (the break-even constant and what
it is derived from, the strict-domination-below-one-block fact, the standing decision); every
number, table and caveat lives here. Measurement discipline is uniform across the sections below:
the deterministic counts are asserted in the driving test and are identical on every run; a pause
figure is a single-threaded, in-process signal that is **printed and never asserted**.

Sections:

* [The allocation-heavy workload](#the-allocation-heavy-workload) — a long process-like loop.
* [The long-lived break-even sweep](#the-long-lived-break-even-sweep) — the switch point the
  automatic rule reads.
* [The two-axis selection curve](#the-two-axis-selection-curve) — object-size distribution ×
  configured mature capacity.
* [The multi-workload measurement](#the-multi-workload-measurement) — the original favourable gate
  plus three shapes.
* [Heap-sizing measurements](#heap-sizing-measurements) — why the fresh mature space is sized from
  the exact live set and the nursery from the surviving set.
* [The nursery chain: indexed containment and the doubling ladder](#the-nursery-chain-indexed-containment-and-the-doubling-ladder)
  — the `O(log #spaces)` range test, the doubled growth step, and the measured `compile:file/2` curve.
* [The debt signal and the collection](#the-debt-signal-and-the-collection) — why only a collection
  reclaims the chain, and why the debt is an opt-in signal.
* [End-to-end RSS and latency](#end-to-end-rss-and-latency-ets_copy-a-b-at-this-commit) — the same A/B
  binary pair on a real `ets_copy` loop and on the `run` path's own loop probe.

## The allocation-heavy workload

Source: `tests/allocation_heavy_workload.rs`, run from the repository root with

```
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc \
    --test allocation_heavy_workload -- --nocapture
```

The workload class none of the other evidence files covers: a **long, allocation-driven run** with
a steady fullsweep cadence. The deterministic counts are **identical on every run**; the pause
column is not (see [Caveats](#caveats)).

### The workload

`ROUNDS = 8000` rounds (200 warm-up + 7800 measured). Each round allocates:

| element | geometry | role |
|---|---|---|
| long-lived set | 160 tuples of 15 elements = 16 words each = exactly one Immix line → **10 whole Immix blocks** (2 560 words) | allocated once in round 0, kept alive through every round; promoted by the first collection and then **never re-copied by Immix** |
| short-lived set | 512 tuples of 2 elements = 3 words each (1 536 words) | reachable during the round's collection (so a minor *promotes* it), then dropped — **dead before the next collection** |
| nursery garbage | 2 048 tuples of 2 elements (6 144 words) | allocated and never referenced — the pure garbage a real allocator produces; dies before the round's own collection |

Over the run that is **20 480 000 short-lived tuples / 61 440 000 words** allocated and
reclaimed — genuinely allocation-heavy — against a bounded 2 560-word long-lived set.

Collections: `fullsweep_after = 2`, i.e. **two minors then a fullsweep** (every 3rd
collection; the cadence is asserted). Identical heap policy for both strategies
(`initial_young_words` 16 384, `initial_mature_words` 32 768, `min_heap_words` 32 768 — the
configured mature generation is well above the automatic rule's break-even
`IMMIX_BREAK_EVEN_WORDS` = 2 048, so the mark-region strategy is genuinely exercised). The
policy pins both spaces at a fixed point of `HeapPolicy::mature_after_fullsweep`, so every
fullsweep reclaims **in place** for Immix (no relocation) and the capacities are a constant.

### Deterministic results

Both strategies, 8000 rounds (identical collection schedule: 5 334 minors / 2 666 fullsweeps):

| metric | Copying | Immix (mark-region) | Immix ÷ Copying |
|---|---|---|---|
| objects moved (over the run) | 4 522 720 | 4 096 160 | 0.906× |
| **words moved (over the run)** | **19 115 520** | **12 290 560** | **0.643×** (−35.7 %) |
| words moved per round (mean) | 2 389 | 1 536 | — |
| peak mature occupancy | 7 168 words | 27 136 line-words (1 696 lines) | **3.79×** |
| peak resident words (nursery + mature) | 14 848 | 34 816 | 2.35× |
| steady-state nursery capacity | 16 384 words | 16 384 words | 1.00× |
| steady-state mature capacity | 32 768 words | 32 768 words | 1.00× |
| words reclaimed (see unit caveat) | 61 435 392 words | 114 663 424 line-words | 1.87× |
| **pause median of 5 forced fullsweeps** | **54–57 µs** | **95–126 µs** | **1.7–2.3×** ← signal, not asserted |

Derived, and asserted as an exact identity by the test:

* `Copying − Immix = 2 666 fullsweeps × the long-lived set (160 objects / 2 560 words)`
  = **426 560 objects / 6 824 960 words**. That difference *is* the whole story: on this
  workload the two strategies do **identical work on the nursery** (the same live young set,
  copied word for word — 5 333 minors × 1 536 words + round 0's 4 096 = 8 195 584 words) and
  reclaim an
  identical **32 772 096 words of nursery garbage** (6 144 per minor, the allocation-heavy
  half). Every divergence comes from the mature generation, where a copying fullsweep
  re-copies the whole live set into a fresh space while the mark-region fullsweep reclaims in
  place.
* The mark-region fullsweep **pins all 160 long-lived objects and evacuates 0** (asserted at
  every fullsweep): the long-lived set occupies whole Immix blocks, so it is `AllMarked` and
  never moves; only the fully-free blocks of dead short-lived batches are reclaimed.
* The peak occupancy is `fullsweep_after + 1 = 3` short-lived batches on top of the
  long-lived set, for both: `2560 + 3×1536 = 7168` exact words (Copying) vs
  `(160 + 3×512) × 16 = 27 136` line-words (Immix) — the line-granular mark-region space
  charges every 3-word tuple a whole 16-word line (5.3× its payload).
* Structural equality: after the last collection the surviving 160-tuple long-lived set is
  `beam_term::compare`-equal under both strategies, still holds its original values, and its
  independently measured `size_object` footprint is the expected 2 560 words.

### Verdict

**Immix measurably wins the copy-work axis; Copying stays the right default.** On this
workload class — a *modest bounded long-lived set* plus a *large volume of short-lived
allocations that die before the next collection* — the mark-region mature generation moves
**35.7 % fewer words over the run** (−9.4 % fewer objects), and the saving is entirely the
long-lived set it pins instead of re-copying at each fullsweep. It pays for that with a
**3.79× larger peak mature footprint** (line-granular allocation: a 3-word tuple occupies a
16-word line) and, on the measured signal, a **1.7–2.3× longer fullsweep pause** — the
mark-region reclaim walks the whole root set and every line of the region, and a fullsweep
walks the nursery twice (`scan_young` then promotion), where a copying fullsweep only touches
the live words it moves.

Read against the crate's standing decision, the measurement **confirms rather than overturns**
it:

* the win is on **copy work**, and it is bounded — it is exactly `fullsweeps × long-lived
  set`, capped by how often the cadence fires and how large the long-lived set is. Halving
  the fullsweep rate halves the absolute saving; on a long-lived-heavier process it grows;
* the loss is on **memory** (line rounding is a function of the short-lived object size, not
  of the strategy) and, on this implementation, on **pause**;
* so for a process where the mature generation is only *modestly* long-lived and the workload
  is dominated by short-lived allocation — the shape measured here — the copy-work saving does
  not buy back the footprint and latency it costs: **`MatureKind::Copying` remains the right
  default**, and Immix remains a size-/latency-tolerant option selected by the automatic rule
  (≥ 2 048 configured mature words) or an explicit `GcConfig::immix` pin, exactly as
  `src/selection.rs` and the crate `CONTEXT.md` record.

A process that is **long-lived-heavy** (a large mature generation re-copied by every
fullsweep) is where Immix's advantage accumulates; that regime is the one measured in the
[next section](#the-long-lived-break-even-sweep).

## The long-lived break-even sweep

Source: `tests/immix_break_even.rs`. Re-measures the gate with a **single-variable sweep**: a
purely long-lived corpus (every object a live root) at object points
`[4, 8, 16, 32, 128, 512, 1024]` = 0.25 → 64 whole Immix blocks, each forced through a fullsweep
under **both** strategies, in three shapes (`Line`, `Small`, and `Multi` = a 17-word tuple
spanning two lines, the multi-line analogue of the worst case). The two decisive shapes are:

* `Shape::Line` — 16-word tuples, exactly one Immix line per object, so the mark-region footprint
  equals the copying footprint (an apples-to-apples comparison of the *copy work*);
* `Shape::Small` — 3-word tuples, the line-granular worst case (3 useful words per 16-word line).

Per point: `need_reserve_words 64`, `initial_young_words = live_line_words + 4096`,
`initial_mature_words = 2*(live_line_words + 64)` (the `mature_after_fullsweep` fixed point),
`fullsweep_after 65535`, so the forced fullsweep lands in the mark-region space. Measured — the
counts are **exact** and asserted; timings are printed by the test, never asserted:

Per point (moved words copying → immix; counts exact): below one whole block Immix is never fewer
and is strictly more for the small-object shape — 0.25 blk `64 → 64` (Line) / `12 → 64` (Small),
0.5 blk `128 → 128` / `24 → 128`; from **one whole block up** Immix pins the whole live set and
moves **0** (1/2/8/32/64 blocks → 16/32/128/512/1024 pinned) while copying re-copies the entire
live set. Footprint c/i 1.0× (Line) / 0.19× (Small) — both pure geometry. The exact per-point
tables are printed by `tests/immix_break_even.rs` (the deterministic counts are also asserted).

### Derived rule — `select_mature` switches at 8 whole blocks (`IMMIX_BREAK_EVEN_WORDS = 2048`)

The constant is a **copy-work** break-even, read off the deterministic counts:

1. **Below one whole block Immix is strictly dominated** — it has no full block to pin, so it
   moves *more* words than a copy (line rounding: 5.3× more for the small-object shape) and its
   resident footprint is 5.3× copying's; the rule must never select it there.
2. **From one whole block up it pins the whole live set (0 moved words)** and its resident
   footprint is an **object-size-independent multiple of the live set** (line-granular: 1.0× when
   the objects are line-sized, up to 5.3× for 3-word objects) — no larger configured size changes
   that geometry.
3. **8 whole blocks (2048 words) is the selected switch point** — the smallest power-of-two block
   count in the regime whose deterministic counts are already flat, so the rule reads a
   copy-work/geometry break-even.

The wall-clock pause is printed by the harness but is **not reproducible as a stable figure and
is therefore never asserted**. The switch point also (a) sits 8× above the strict-domination
regime, (b) matches the independent three-shape measurement above ("roughly ≥ 8 full blocks"), and
(c) leaves the **default policy's 233-word mature generation** (under one block) on the copying
path — so `GcConfig::default()` resolves to the copying collector.

* **The rule reads the *configured* mature size and is evaluated once.** `mature_words_of(policy)`
  is the collector's construction-time `initial_mature_words`; a collector keeps its strategy for
  life (re-selecting would mean rebuilding the mature space mid-life), so a heap that *grows* past
  the break-even later is not re-evaluated — a caller that knows its mature generation will grow
  large pins `GcConfig::immix` up front.
* **Explicit override + observability.** `MatureSelection::Explicit(kind)` beats the rule in both
  directions (`GcConfig::copying`/`GcConfig::immix`/`GcConfig::with_mature`).
  `GcConfig::mature_choice()` reports the exact source; `AnyCollector::mature_choice()` recovers it
  from the live collector (a pin that agrees with the rule is reported as `Automatic`);
  `MatureChoice::describe()` renders the decision for logs / `system_info`-style output. Covered by
  `tests/mature_selection.rs`.
* **Honest caveat.** The mark-region path's *pause* is worse in the regime where it is selected, so
  the rule trades latency for copy work; a pause- or memory-sensitive process should pin
  `MatureSelection::Explicit(MatureKind::Copying)` regardless of size. The break-even is a
  **copy-work** break-even, not a wall-clock one; only the deterministic counts (pinned/moved
  objects+words and the `Law`) are asserted, and the pause signal — a single-threaded in-process
  micro-benchmark — is printed, never asserted and not reproducible run-to-run. Read the *value* of
  `IMMIX_BREAK_EVEN_WORDS` as geometry-motivated: below one whole block Immix is strictly
  dominated, and from one whole block up the pinned/moved split and the footprint multiple are
  fixed by the geometry.
* **Standing decision (re-confirmed by the two-axis sweep).** `Auto` **never** selects Immix for
  the default policy — the rule reads the construction-time `initial_mature_words` (default 233 <
  one 256-word block), so the first size it accepts is 2048 configured words, which a
  default-policy process can never reach however large its mature generation grows later. Where
  Immix *is* selected it moves 2.8×–22× fewer words (a deterministic count) while pausing longer,
  so **`MatureKind::Copying` stays the default**; Immix is reachable only by an explicit pin or a
  configured mature generation ≥ 2048 words.

## The two-axis selection curve

Source: `tests/immix_selection_curve.rs`. Extends the gate to **two further axes** (deterministic
counts asserted, pauses printed only, never asserted).

* **Axis 1 — object-size distribution** at a fixed in-place mature size, five shapes (cons 2w /
  small tuple 3w / line tuple 16w / multi tuple 17w / mixed): Immix evacuates only the trailing
  **partial-block** lines, so its moved words are **distribution-invariant at 96 words** for every
  shape while copying's cost tracks the payload (268→2144 words); its resident footprint ranges
  **1.0×–8.0×** copying's.
* **Axis 2 — configured mature capacity** 128→8192 words at a fixed live set (occupancy 62.5%→
  22.7%): Immix's moved words and footprint are **flat** across the whole range.

Net: the constant is a **geometry-driven copy-work** break-even (whole-block pinning +
line-rounded trailing evacuation), insensitive to occupancy and object-size distribution — no
point below 2048 words becomes favourable, none above becomes unfavourable.

## The multi-workload measurement

Sources: `tests/immix_measure.rs` (the original single, favourable workload),
`tests/immix_workloads.rs` (three shapes through both strategies, `-- --nocapture`),
`tests/immix_selection_curve.rs` (the two axes above).

`tests/immix_measure.rs` — 128 long-lived one-line tuples (8 full Immix blocks) plus 16 dropped /
8 kept transients and 300 nursery garbage tuples, forced fullsweep: copying re-copies **136
objects / 400 words** vs Immix **moves 8 objects / 128 words and pins 128** (384 words) — a real
movement win, at a footprint cost (400 live words → 2176 resident line-words).

`tests/immix_workloads.rs` — three shapes: long-lived shared subterm (copying 129 obj / 639 w vs
Immix **0** moved / 129 pinned), garbage-heavy small live set (10 obj; copying 20 w vs Immix 160
line-w), and large-heap promotion pressure (copying 633 obj / 1824 w vs Immix 121 obj / 288 w,
10128 resident line-w). Both strategies find the **same** live set (`evacuated + pinned ==
copied`) and reclaim identical garbage.

**Decision — the copying space is the default; GenImmix is a large-live-set regime selected
automatically.** GenImmix wins the copying work on long-lived data, but every win is paired with a
loss: (a) **footprint** 3.6–8× (line-granular internal fragmentation); (b) **pause** worse wherever
it saves work (the reclaim walks the whole root set and every line, and the major walks the nursery
twice: `scan_young` then promotion); (c) an outright loss on the common small-heap/high-churn shape
(8× the moved volume for the same 10 objects). Pause is an in-process median over forced
fullsweeps — a signal, not a benchmark; every *assertion* is on the deterministic counts.

## Heap-sizing measurements

The two size-policy rules the collector's per-collection re-size implements, measured — they are
what fixes the *destination sizing* in Design Decisions.

* **The fresh mature space must be sized from the EXACT reachable live set, never from
  pre-collection occupancy.** Occupancy is garbage-inflated, so sizing from
  `young.used_words() + mature.used_words()` fires the policy's grow test
  (`should_grow`, `3*capacity < 4*need`) on *every* fullsweep and the mature capacity climbs without
  bound: over a 200-round allocate/collect loop at the default cadence with a 60-word live set the
  capacity ran **`233→376→610→987→…→6772`** (Copying) and up to **`28928`** (Immix). Sizing from
  `live.rs`'s exact reachable live count converges the **same** loop to a constant **`233`**
  (Copying) / **`512`** (Immix), matching the C reference (`erl_gc.c:2072`).
  Evidence: `tests/steady_state.rs`, `tests/mature_capacity_convergence.rs`.
* **The nursery must be re-sized from the SURVIVING set, at offset 0.** A nursery is only collected
  when it is *full*, so sizing it from its pre-`reset()` fill (≈ capacity) and stepping offset-1
  `next_words(capacity.max(request), 1)` both fire on *every* collection, growing it ~20 % per
  collection with no bound — RSS then scales with the workload size. On the baseline `foreign`
  workload that regime gives young `heap_size` **987 → 833 026** (N=1000) → **2 487 399** (N=5000)
  → **7 427 328** (N=20000) with Rust `VmHWM` **29.4 / 77.3 / 227.5 MiB** against the oracle's flat
  **~60 MiB**. Under the bounded rule the nursery is *flat* — a 200-round growing load holds it at
  its **233**-word default while the total heap plateaus at **609** (Copying) / **1257** (Immix)
  words. Evidence: `tests/nursery_bounded.rs`.

## The mature-generation room policy (generational cadence)

The fullsweep sizes the fresh mature space from `live.rs`'s **exact reachable live word count** *plus*
the **young heap's occupancy** (`nursery.used_words()`) — the C `adjust_after_fullsweep`
`need_after = (HEAP_TOP(p) - HEAP_START(p)) + need + …` (`erl_gc.c:2072`). This is the measurement that
gates that policy; the alternative (passing `0` for the young occupancy) is **dominated** and is why the
policy is what it is.

**Workload.** A churn loop: each round allocates a fresh bounded live graph of 20 `{a, 2a}` tuples
(60 words) plus 50 garbage tuples (150 words) and then collects — 20 warm-up + 200 measured rounds,
at the crate-default `HeapPolicy` (`fullsweep_after` 65535, 233-word young / 233-word mature,
`need_reserve_words` 64). Deterministic counts only.

| | Copying, before (`need = 0`) | Copying, after (`need = young`) | Immix @default | Immix @`initial_mature_words` 1024 |
|---|---|---|---|---|
| minors / fullsweeps | 1 / 219 | **147 / 73** | 0 / 220 | **147 / 73** |
| collection-kind cycle | every collection a Major | `Minor, Minor, Major` | every collection a Major | `Minor, Minor, Major` |
| longest minor run | 0 | 2 | 0 | 3 |
| young capacity (constant) | 233 | 233 | 233 | 233 |
| mature capacity (constant) | 233 | **376** | 512 | 1024 |
| total heap (`heap_info`) | 466 | **609** | 745 | 1257 |

With `need = 0` the fresh mature is never large enough to receive a nursery's worth of promotion, so
**every collection after the first escalates to a fullsweep** and `minor_gcs` stays 0 — the generational
payoff (promote survivors into a mature generation, fullsweep it only on the `fullsweep_after` cadence)
is never realized. Folding the young occupancy in makes the mature region able to hold the next round's
promotion, so minors actually run and dominate the cadence (**147 vs 73**).

**Bounded, not creeping.** Both capacities are a single constant across the 200 measured rounds. The
young occupancy is capped by the nursery capacity, which `young_after_collection` itself keeps bounded,
so the grow branch's target `next_words(need_after, 0)` is a fixed point (376 for the Copying default,
1024 for the Immix policy above) — memory does **not** grow per collection. Using the **mature**
occupancy instead would be garbage-inflated and creep without bound; that is why only the young
occupancy is folded in.

**Immix at the crate default.** The default mature region converges to 512 words = 32 lines, which holds
the 20-line live set and leaves only 12 free lines — fewer than the ~20 a round promotes — so at the
*default* policy Immix still escalates every round (a region-granularity consequence, not a sizing
regression). Given room for the promotion (`initial_mature_words` 1024 = 64 lines) Immix runs the same
147/73 cadence as Copying and its region stays a bounded 1024 words.

**Evidence:** `tests/generational_cadence.rs` (asserts the cadence, the `minor_gcs` advance/reset, live
term survival, reclamation and the converged constants for both strategies), `tests/steady_state.rs`
(pins young 233 / mature 376, total 609, Copying; 233 / 512, total 745, Immix),
`tests/mature_capacity_convergence.rs`, `tests/nursery_chain.rs`. A large off-heap loop at a 4096-word
young policy converges to Copying 4096 / **4185** (9 fullsweeps / 231 minors) and Immix 4096 / 4352.

## Caveats

* **The pause column is a signal, not a benchmark.** It is a single-threaded, in-process
  median of 5 forced fullsweeps timed with `std::time::Instant`, with no warm-up control, no
  cross-process isolation and allocator/scheduler noise; the medians observed for the
  allocation-heavy workload were `54/57/57 µs` (Copying) and `95/124/126 µs` (Immix) over three
  runs. It is **not reproducible run-to-run** and is **never asserted**. Only the
  copying-vs-mark-region ratio on the same shape carries information. The host may be under load,
  so only the deterministic counts and their ratios should be quoted as results.
* **The `words reclaimed` row is not unit-comparable across strategies.** The copying figure
  counts exact 3-word objects; the mark-region figure counts whole 16-word lines, so it
  over-states the same logical garbage by the same line-rounding factor as the footprint. The
  *nursery-garbage* half is identical for both (32 772 096 words over the minors, asserted).
* **Sizes are asserted, not tuned per strategy.** Both strategies run the *same* pinned
  policy; Immix is simply given room for its line-rounded occupancy (32 768 words = 128
  blocks). A tighter upper bound on the mark-region space would change when it has to
  relocate, not the per-collection counts measured here.
* Every deterministic figure above is asserted in its driving test; this file records what those
  tests print, and nothing here is asserted anywhere else.

## The nursery chain: indexed containment and the doubling ladder

**Containment is indexed, not scanned.** `Nursery::contains` — the predicate a host's *unscanned-root* valve
evaluates **per reference word and per heap admission** — answers from an ordered index of the retired
spaces' live regions plus one direct test of the current space, so it is `O(log #spaces)` rather than a walk
over every space.
The index is exact: every space is a **distinct live allocation**, so the intervals `[base, top)` are
pairwise disjoint; the greatest base at or below the probe is the only candidate, decided by `addr < top`
(the same half-open rule as `HeapSpace::contains`).
An entry never goes stale: nothing is ever allocated in a retired space, so its `top` is **frozen** from the
moment it is retired until `collapse` drops it (and clears the index with it); the current space is the only
one whose `top` moves, and it is tested directly instead of being indexed.
**Growth installs at least a doubling.** `Nursery::grow` takes `request = need_words.max(2 * capacity)` and
installs `policy.next_words(request, 0).max(capacity)`, so a growth loop repeating a **constant** request
climbs the growth table geometrically and the chain stays `O(log(heap))` long rather than gaining one space
per call.
**The two other per-reference range scans were narrowed with it.** `FromSet` keeps its ranges base-sorted
with one binary-search `locate`, and `reachable::Visited::index` delegates to it, so neither is a linear scan
over the ranges.

**The measured curve, on the route that exposed the defect.** A tiny probe module (compiled by the system
`erlc`) whose `go/0` calls `compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}])`
with `Source`/`Dir` baked into the probe source (the `run <mfa>` route takes integer args only), staged per
module and run as `erts_rust/target/debug/beam_rt run probe.beam probe:go/0`; GNU `time -v` for the wall
clock, CPU% and max RSS. Real `lib/stdlib/src/` modules. **BEFORE** is the pre-fix baseline measured in a
sibling worktree on this host with the same probe route (`beam_gc` at `0bb28ed31c`); **AFTER** is
`beam_gc` at `29fee9aef1`. Host: 14 cores, `/proc/loadavg` 116–237 during the runs (recorded per row), so
the absolute wall clocks are upper bounds and only the ratios carry information.

| module | lines | wall BEFORE | wall AFTER | loadavg (BEFORE → AFTER) | CPU% AFTER | max RSS AFTER | `Code` chunk (md5, unchanged?) |
|---|---|---|---|---|---|---|---|
| `erl_abstract_code` | 53 | 45.50 s (31.81 s on a quieter run) | **8.22 s** | 130 → 130 | 70 % | 314 520 kB | 403 B `14b1f6140d1b98f4b59f5db63dc70669` **yes** |
| `erl_posix_msg` | 178 | 36.85 s | **13.16 s** | 130 → 129 | 93 % | 539 984 kB | 1943 B `d1cfaed899a8201687c3602f866f775f` **yes** |
| `pool` | 300 | 144.93 s | **15.90 s** | 129 → 137 | 83 % | 365 356 kB | 2016 B `edc120b021cc583a0908b6ddaa2989a2` **yes** |
| `ordsets` | 581 | 254.25 s | **19.63 s / 24.68 s** | 145 → 154 (2nd rep 154 → 154) | 84 % / 78 % | 333 672 kB | 1219 B `4f86664f52128d47d5eed339d27c4263` **yes** |
| `timer` | 1017 | **DNF** — `timeout 600` killed it at 600 s (RSS 469 MB, climbing) | **108.83 s / 130.35 s** | 154 → 133 (2nd rep 195 → 205) | 85 % | 746 016 kB / 746 776 kB | 5386 B `12f604e7d31c5e0df585026772f67647` **yes** |

Speedups: **×5.5 / ×2.8 / ×9.1 / ×12.9 / >×5.5** (`timer` bounded below by the 600 s timeout).
`ordsets` was re-run for a noise band: 19.63 s and 24.68 s (the host load is shared; both reps are reported).
**Point-to-point ratios of the AFTER wall clock** — the scaling shape: 53→178 lines (×3.36 lines) ⇒ **×1.60**;
178→300 (×1.69) ⇒ **×1.21**; 300→581 (×1.94) ⇒ **×1.23**; 581→1017 (×1.75) ⇒ **×5.54**.
So the containment fix removes the **quadratic** term module size used to buy (581 lines 254 s ⇒ 20 s, and the
1017-line case that never terminated now completes in ~110 s), and the small modules are now dominated by a
fixed ~7 s floor.
What is **left is not linear** — the last ratio is ×5.54 for ×1.75 lines; the remaining superlinear term is
**not** the nursery (see the profile below) but `beam_rt`'s own whole-graph unscanned-root descent.

**Byte-identity is unchanged.** Every produced `.beam`'s `Code` chunk equals the system `erlc +deterministic`
artifact's for the identical source, byte for byte (`beam_lib:chunks/2` + `erlang:md5/1` via `escript`) — every
md5 above is **identical to the pre-fix baseline's**, including `timer`'s.
The produced `ordsets` and `timer` artifacts contain **0** occurrences of their own outdir (the
`deterministic` stripping) — checked with `grep -c` over the artifact bytes.
The official harness case agrees: `differential run selfhost/in_process_compile_real_stdlib_module_byte_identical`
reports **`agree`** (`total=1 agree=1 mismatch=0 unsupported=0`, contracts A.2+A.9; oracle = the system
`erl` 29.1.1/erts-17.1 on `$PATH`).

**The profile: `Nursery::contains` is gone from the top.** `perf record -F 99 -g` on the fixed `ordsets` run
(2 480 samples) against the baseline profile (11 112 samples), self cost:

| frame | BEFORE | AFTER |
|---|---|---|
| `<beam_gc::nursery::Nursery>::contains` and its `any(space.contains)` fold | **60.94 %** | **1.44 %** |
| … the fold's inlined `BTreeMap` range probe (`search_tree_for_bifurcation`/`find_leaf_edges_spanning_range`/`find_lower_bound_index`/`perform_next_back_checked`) | — | ≈5.2 % |
| `<beam_rt::context::gc_walk::Visited>::insert` | 3.62 % | **44.50 %** |
| `<beam_rt::context::gc_walk::Walk>::run` | 2.40 % | 10.85 % |
| `beam_term::layout::for_each_child_slot` | 1.23 % | 8.22 % |
| `beam_rt::context::gc::root_is_unscanned` / `any_root_is_unscanned` | 0.75 % / 0.45 % | 6.00 % / 3.97 % |

The new top frame is **not** the nursery: it is `beam_rt`'s own whole-graph **unscanned-root descent**
(`gc_walk`'s `Visited`, an open-addressing table keyed directly on the object address), which walks the
process's live object graph at every heap admission and is therefore still superlinear in the live set.
The containment fix **exposed** it rather than created it: a descent that took ~4 % of a ~112 s run is now
~45 % of a ~20 s one.
It is `beam_rt`'s to bound; nothing in `beam_gc` bounds a whole-graph walk.
Caveat (unchanged from the baseline): the perf map is empty, so JIT frames show as raw addresses in both
profiles — every frame above is native Rust, and raw addresses appear only inside the inlined closures.

**The chain's retained words (same-commit A/B on the runtime's no-collection route).**
The compiler's `run` process is **never collected** (`minor_gcs = 0` throughout, `fullsweep_after` 65535), so
this column is the nursery chain's retained capacity plus the (never-grown, 233-word) mature space, read as
`process_info(P, memory)`:

| module | lines | `memory` BEFORE | `memory` AFTER | `heap_size` AFTER (words) | max RSS BEFORE → AFTER |
|---|---|---|---|---|---|
| `pool` | 300 | 61 680 376 B | 77 423 712 B | 9 677 698 | 366 056 kB → 365 092 kB |
| `ordsets` | 581 | 37 899 552 B | 77 423 704 B | 9 677 704 | 331 156 kB → 333 852 kB |
| `timer` | 1017 | DNF (>600 s) | 708 313 928 B | 88 538 974 | — → 746 776 kB |

Read this as **what the doubling rule does and does not buy**, honestly. The retained total is a *geometric*
function of the requirement — ≈ twice the last installed space, so `O(requirement)` and independent of the
workload at a given requirement (`pool` and `ordsets` land on the same 9.68 M words) — whereas the pre-fix
rule (one growth-table step, ≈ ×1.618) retained 7.71 M and 4.74 M words for those two. So the doubling's
dividend is the chain's **length** (`O(log(heap))`, which is exactly what `contains` pays, and which bounds a
constant-request growth loop's chain length too) and **not** a smaller retained byte total: at these sizes the
doubling retains **up to 2.0× more words**, and the peak RSS is
unchanged within noise (331→334 MB, 366→365 MB — the ~300 MB VM/arena baseline dominates). The 2× is the
ladder's overshoot: a doubling space is installed to cover a request it does not shrink back to without a
collection, and nothing collects on this route.

## The debt signal and the collection

**The question.** `Nursery::grow` is the *no-relocation* growth seam: it never collects and never moves a
term, so the space it supersedes must stay addressable until a collection evacuates it — nothing in
`beam_gc` frees a retired space outside a collection's `Nursery::collapse`, and it never will: a
superseded space may hold live words (they were handed to the mutator) and only a collection proves them
dead. Since `collapse` runs **only** inside a collection, the chain is bounded **iff** something asks for a
collection after a growth. `should_collect(need_words)` reports **occupancy only** (the current space's
free words), so the claiming signal is exposed separately and **opt-in**:
`collection_debt()` on `Collector` / `GenImmixCollector` / `AnyCollector`, `true` while the chain carries a
retired space. A `true` answer means *a collection is required to reclaim the chain — a heap-admission door
should treat it like a full nursery*. It costs a caller that ignores it nothing (one `Vec::is_empty`).

**The corpus (in-crate, load-invariant).** A grow + heap-admission cycle repeated 400 times: the engine's
`test_heap` prefix asks the admission question and collects over its published roots when the answer is
true; the op's own un-rooted result is a 256-word tuple (exactly one whole mark-region block), allocated
through the grow-only seam when it does not fit. Driven through **both** mature strategies by
`tests/nursery_growth_retention.rs`. Deterministic; the whole 5-test file reports **0.01 s**, and no timing is
asserted.

| quantity the 400-cycle corpus pins | peak | bound asserted |
|---|---|---|
| chain length (spaces) | **2** | `peak_spaces == 2`, `tail_spaces == 2` — one retired space at most |
| retention (capacity words, peak) | 932 = the largest single space the run installs | `peak_retained <= peak_young`, `tail_retained <= tail_young` |
| current nursery (words, peak) | 932 | `peak_young <= 16 × OP_WORDS` (= 4 096, against the 400 × 256 = 102 400 an iteration-linear leak shows) |
| mature capacity | one constant over the whole tail | identical on Copying and on Immix |

**The chain's LENGTH is the bounded quantity, not a per-call step.** `grow_nursery` installs at least a
**doubling** (the mechanism above), so a growth loop repeating a **constant** request climbs a geometric
ladder: the chain is `O(log(heap))` long and the retained words are the ladder's geometric sum, never a
function of the iteration count. Only a collection's `collapse` frees a retired space, and it collapses the
chain to **exactly one** space.
`tests/nursery_growth_retention.rs::a_repeated_grow_and_collect_cycle_never_accumulates_spaces` pins that over
400 explicit grow + collect cycles, and `tests/nursery_chain.rs::a_grow_then_collect_loop_reaches_a_bounded_steady_state`
pins the plateau.

**Cost.** The debt query itself adds **no per-allocation work**: the nursery bump path is untouched (the
growth check runs only where the allocator already branched to grow), and the only added work is **one
collection per growth episode** at the *next* heap-admission point — the correct generational cadence, not a
new tax. Folding the debt into `should_collect` instead taxes **every** admission whether or not the door
ends up collecting, which is why the debt is the separate, opt-in `collection_debt()` accessor and
`should_collect` reports occupancy only.

**Superseded spaces are never freed outside a collection (deliberate).** A growth retains its superseded
space whatever its occupancy, including one that never handed the mutator a word — dropping that
provably-empty case would be sound but changes `young_space_count()`, so it is deliberately not done.
Pinned by `a_superseded_space_is_retained_and_never_freed_by_a_growth` and by `tests/nursery_chain.rs`.

**Mark-region contiguity: promotion is now ATOMIC and the collector RECOVERS.** Driving the same cycle
with a **100-word** (7-line) tuple at a 2048-word region used to fail after **43 rounds** with
`Alloc(AllocFailure { requested: 800, kind: Heap })` while `mature_capacity_words()` still reported 2048.
Cause: `ImmixSpace::alloc_words` needs a run of the object's 7 lines **inside one 16-line block**
(`find_run`/`first_hole`), while the promote/reclaim pre-check compared against the region-wide
`free_lines()`; two 7-line objects fill 14 of a block's 16 lines, so the region reports free lines no
single run can use. The promotion used the shared `forward_slot` marker protocol and therefore failed
**mid-copy** — a half-forwarded heap surfaced as a spurious allocation error.

Both halves are now closed, deterministically:

* **Atomic.** `immix_collector::promote` runs in two phases: a **reserve** phase that copies and marks but
  rewrites no reference word, and a **rewrite** phase applied only once every object of the traversal has
  been placed. A reservation that finds no run ends phase R, each source is restored from its own copy
  (`copy_nonoverlapping(dst, src)`) and each reservation un-claimed through the new
  `ImmixSpace::release_words` (the documented inverse of `claim`: clear the `allocated` bits, take a block
  whose bitmap emptied back to `BlockState::Free`), so the from-space is **byte-identical** afterwards —
  no marker, no rewritten root, no counter advanced. Measured: with the region packed to
  **112 used / 16 free lines** (8 blocks x 14 lines, `can_reserve(100) == false`) a `collect_minor` of two
  fresh 7-line tuples reports the typed `Alloc` error with `used_words()`, `used_lines()` (112),
  `free_lines()` (16) and the full nursery word image unchanged, and a second attempt reproduces it
  exactly (`tests/immix_fragmentation.rs::a_refused_promotion_leaves_both_spaces_byte_identical`).
* **Recovering.** `GenImmixCollector::collect` escalates a refused minor to a **fullsweep** (reusing the
  `needed`/`refs` its single `scan_young` already produced); `run_major` falls back to `grow_major` on a
  refusal as well as on `free_lines() < needed`; and `grow_major` now also rebuilds a **fresh region of the
  current capacity** when a `max_heap_words` ceiling forbids growing — an empty region packs monotonically,
  so the run the fragmented in-place region could not produce is exactly what it has. So the failure is
  simply gone: 64 rounds of two 7-line tuples at a 2048-word region reach a steady state of
  **capacity 1024 words / peak 112 used lines / 14 fullsweeps / 50 minors**, and 48 rounds of 129-word
  (**9-line**, one object per block) tuples reach **capacity 2048 / peak 72 used lines / 11 fullsweeps** —
  identical on both the concrete collector and the `AnyCollector` seam, with every root
  `beam_term::compare`-equal to an independently built witness after every round. The same counts are
  deterministic round to round (no timing is asserted anywhere).

**Residual sharp edge (honest).** Only a policy that *forbids* growth **and** a live set no fresh region of
the current capacity can hold (`live_lines > line_count`) still reports the typed heap exhaustion — and
then still **before any word is copied**. That is genuinely unsatisfiable, not a fragmentation artefact, and
is what `tests/immix_major_growth.rs` (cap 256 on a one-block region with 20 live lines) and
`src/immix_collector/grow_tests.rs` pin unchanged. `collect_minor`'s **own** public contract also still
reports the typed error for a fragmented region rather than escalating — escalation is `collect`'s job.

**Caveats.**
* **The external RSS/latency figures are in [the end-to-end section below](#end-to-end-rss-and-latency-ets_copy-a-b-at-this-commit).**
  On a real `ets_copy` loop the process RSS is **flat** on both binaries of the pair — the loop's own heap
  admissions collect, so no chain accumulates there — and a retained chain is only reachable through the
  **un-rooted** allocation seam. The in-crate table above is the load-invariant evidence for the mechanism;
  the two are not interchangeable.
* **Retained *words* are a capacity figure, not RSS.** A retired `HeapSpace` holds a whole backing
  allocation, so the chain's **length** (2 spaces in the corpus above) is the RSS-relevant quantity, but the
  two are not numerically interchangeable (allocator carriering, page granularity, and the rest of the
  process's footprint sit on top).
* **A pure un-rooted growth loop still grows the chain** — it never reaches a heap-admission point, so
  nothing claims the debt. That is pinned deliberately by `beam_rt`'s own
  `alloc_words_grows_the_nursery_and_never_falls_back_to_the_arena` (it asserts `young_space_count() > 1`).
  Every real Erlang loop reaches an admission point at its `test_heap` prefixes, where the runtime's door can
  consult `collection_debt()` and collect.
## End-to-end RSS and latency (`ets_copy`, A/B at this commit)
**What was run.** `tools/bench`'s own `copy_bound` loop body, on the **same A/B binary pair** as the sections
above (`beam_gc` at `0bb28ed31c` = BEFORE, `29fee9aef1` = AFTER), as
`erts_rust/target/debug/beam_rt -pa <dir> run bench_copy_bound.beam bench_copy_bound:ets_copy/1 <N>`; wall
clock and peak RSS from GNU `time -v`; `/proc/loadavg` 187–207, `nproc` 14.

| N | max RSS BEFORE | max RSS AFTER | wall ms BEFORE | wall ms AFTER |
|---|---|---|---|---|
| 0 | 144 616 kB | 143 612 kB | 2 960 | **890** |
| 100 000 | 144 620 kB | 143 600 kB | 5 320 | **2 360** |

**Verdict: this loop's RSS is FLAT on both sides** — N=0 and N=100 000 differ by 4 kB BEFORE and 12 kB
AFTER, so there is no per-iteration retention left on this route to bound (consistent with the flat
`memory` the loop probe below measures directly). The only difference is the wall clock: **×3.3 at N = 0**,
**×2.3 at N = 100 000**.
**What this measures.** The process **does** collect on this route — `minor_gcs` advances at ≈ `N/3` (see the
`run`-path probe below) — and the RSS is flat in N on both binaries, so no nursery chain accumulates here and
the `beam_gc`-side change is not the thing this loop's footprint depends on.
**The opt-in debt remains the policy.** `should_collect` reports **occupancy only**; `collection_debt()` is
the separate, opt-in "a collection is required to reclaim the chain" signal, because folding it into
`should_collect` taxes every admission whether or not the door ends up collecting.
**Caveats.** Reconstructed harness (the sibling `tools/bench` N is a compile-time constant and is read-only
here); these figures are a **debug** build, so absolute RSS is not comparable with a release build's — only
the flatness and the before/after wall-clock ratio are carried as results;
the ~144 MB fixed baseline dominates at these N.

## The `run` path collects at its own heap admissions; the forced `garbage_collect()` adds nothing (probe, N = 100 … 20 000)
Source: a `$TMPDIR`-built probe module (`gcprobe.erl`), run on the **same A/B binary pair** as the sections
above (`beam_gc` at `0bb28ed31c` = `prefix`, `29fee9aef1` = `fixed`) as
`erts_rust/target/debug/beam_rt run gcprobe.beam gcprobe:go/1 <N>` (debug build).
The sibling `tools/bench` module's N is a compile-time constant and is read-only here, so the probe replicates
its `ets_copy` shape: `ets:new(set, public)`, then N rounds of building a 5-field payload map
(a 10-element list, a 3-tuple with a small binary and a float, a 12-byte binary, a char list, a negative
integer), `ets:insert/2`, matching `ets:lookup/2`; then `ets:delete/1`, and reads
`process_info(self(), memory)` and `process_info(self(), garbage_collection)` **before and after**
`erlang:garbage_collect/0`.
`/proc/loadavg` 204–220 during the runs, `nproc` 14.

| N | `prefix` MEM before → after | `fixed` MEM before → after | `minor_gcs` before → after (both) |
|---|---|---|---|
| 100 | 16 856 → 28 040 B | 18 008 → 30 792 B | 32 → 32 |
| 1 000 | 16 856 → 28 040 B | 18 008 → 30 792 B | 332 → 332 |
| 5 000 | 18 720 → 29 904 B | 18 008 → 30 792 B | 1 665 → 1 665 |
| 20 000 | 18 720 → 29 904 B | 18 008 → 30 792 B | 6 665 → 6 665 |

**The live heap is flat in N.** On each binary the N = 100 and N = 20 000 figures are the same bytes (the
`prefix` pair moves by 1 864 B, the `fixed` pair not at all), so the `run` process retains **no** per-iteration
memory on this route — it is bounded by the working set, not by the loop count.
**The process collects during the loop.** `minor_gcs` advances ≈ `N/3` on **both** binaries (the collection
cadence of the loop's own heap admissions), so the `run` path is not a route on which collections are refused.
**The forced call contributes no collection.** `erlang:garbage_collect/0` leaves `minor_gcs` exactly where the
loop left it while the reported `memory` rises by its own bookkeeping only (≈ 11 kB), i.e. the forced request
itself changes nothing the loop had not already done — it is not the mechanism behind this route's memory.
**Caveats.** Debug build, and the probe is a different module from any earlier one, so the absolute byte
figures are not comparable across probe revisions — only the flatness in N, the `minor_gcs` slope and the
`before → after` delta are carried as results. Load is shared, so no timing is quoted here.

## Runtime-shaped dispatch (the break-even through the runtime's own GC doors)
Source: `../beam_rt/tests/gc_immix_payload.rs` and `../beam_rt/tests/gc_strategy.rs` (crate `beam_rt`,
outside this crate's own test tree). The same workload driven through `beam_rt`'s own GC doors with a
runtime-spawned process reproduces the small-object/high-churn verdict exactly at the break-even: with
`initial_mature_words` **2047** (Copying) vs **2048** (Immix), Immix moves **1.146×** the words for
**6.57×** the mature occupancy. That is why the automatic rule's default stays Copying and
`MatureKind::Immix` remains an opt-in — `BEAM_RT_GC_STRATEGY=immix` raises the spawned policy's mature
generation past the break-even. Load-invariant counts only; timings are not asserted.
