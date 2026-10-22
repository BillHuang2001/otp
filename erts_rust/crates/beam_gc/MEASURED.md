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
  the exact live set and the nursery from the same whole-heap live set, through one target.
* [The nursery chain: indexed containment and the one-step-then-doubling growth rule](#the-nursery-chain-indexed-containment-and-the-one-step-then-doubling-growth-rule)
  — the `O(log #spaces)` range test, the growth step, and the measured `compile:file/2` curve.
* [The debt signal and the collection](#the-debt-signal-and-the-collection) — why only a collection
  reclaims the chain, and why the debt is an opt-in signal.
* [End-to-end RSS and latency](#end-to-end-rss-and-latency-ets_copy-a-b-at-this-commit) — the same A/B
  binary pair on a real `ets_copy` loop and on the `run` path's own loop probe.
* [Per-op attribution of the `gc_bound/churn` shape](#per-op-attribution-of-the-gc_boundchurn-shape-the-collectors-own-counters)
  — what the churn shape pays per operation.
* [The nursery follows the live set — the live-set nursery target](#the-nursery-follows-the-live-set--the-live-set-nursery-target)
  — one target per collection kind, the counters it moves and the A/B; with
  [the acceptance measurements](#the-acceptance-measurements-a-collections-capacity-is-the-live-set)
  (the `beam_rt` pins), [the CLI flatness probe and the burst
  residual](#the-cli-flatness-probe-and-the-burst-residual), [the rejected nursery-sizing
  variants](#the-rejected-nursery-sizing-variants) and [the cadence against the
  oracle](#the-cadence-against-the-oracle-counts).
* [The off-heap binary roster](#the-off-heap-binary-roster-reachability-reclaim-and-the-binary-virtual-heap-trigger)
  — the reachability sweep flat in N, and the pre-wiring runtime probe that shows what the missing
  trigger costs (one retained 64 KiB payload per call).

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
  `young.used_words() + mature.used_words()` fires the policy's grow bracket
  (`capacity < target`) on *every* fullsweep and the mature capacity climbs without
  bound. Sizing from `live.rs`'s exact reachable live count converges the **same**
  200-round loop to a constant nursery **233** words with mature **987** (Copying) /
  **1 024** (Immix), matching the C reference (`erl_gc.c:2072`).
  Evidence: `tests/steady_state.rs`, `tests/mature_capacity_convergence.rs`.
* **The nursery must be sized from the WHOLE-HEAP LIVE SET, through one target, at offset 0.** A
  nursery is only collected when it is *full*, so a size derived from its pre-`reset()` fill (≈
  capacity), from its own capacity, or from an offset-1 `next_words(capacity.max(request), 1)` step
  fires on *every* collection, growing it ~20 % per collection with no bound — RSS then scales with
  the workload size. On the baseline `foreign` workload that regime gives young `heap_size`
  **987 → 833 026** (N=1000) → **2 487 399** (N=5000) → **7 427 328** (N=20000) with Rust `VmHWM`
  **29.4 / 77.3 / 227.5 MiB** against the oracle's flat **~60 MiB**. Sizing from
  `HeapPolicy::young_target_words` leaves the nursery *flat* wherever the live set is — a 200-round
  growing load whose garbage is all collected holds it at its **233**-word default while the total
  heap plateaus at **1 220** (Copying) / **1 257** (Immix) words, and the same load with a *growing*
  live set drags the nursery up the growth table in live-set-target steps (233 → 1 598 → … → **17
  731**, the live set's own target). Evidence: `tests/nursery_bounded.rs`, `tests/steady_state.rs`.

## The mature-generation room policy (generational cadence)

The fullsweep sizes the fresh mature space from `live.rs`'s **exact reachable live word count** *plus*
the **young heap's occupancy** (`nursery.used_words()`), times `MATURE_TARGET_LIVE_MULTIPLE`:
`HeapPolicy::mature_target_words(live, young) = next_words(2 × (live + young + need_reserve_words), 0)`,
where the un-multiplied sum is the C `adjust_after_fullsweep`
`need_after = (HEAP_TOP(p) - HEAP_START(p)) + need + …` (`erl_gc.c:2072`). This is the measurement that
gates that policy, in both of its halves.

**The young occupancy has to be folded in.** A fresh mature space sized from the live set alone
(`need = 0`) can never receive the next round's promotion, so every collection after the first
escalates to a fullsweep and `minor_gcs` stays 0: the generational payoff (promote survivors into a
mature generation, fullsweep it only on the `fullsweep_after` cadence) is never realized. Folding the
young occupancy in is what makes minors run at all.

**The multiple has to be applied to that sum, and the mature *occupancy* must never be.** One
`need_after` admits exactly one promotion, so a space of exactly `need_after` runs a single minor
between two fullsweeps; `MATURE_TARGET_LIVE_MULTIPLE = 2` is what turns that into a **run** of minors
(measured below: 12 on the copying default). The occupancy, by contrast, is garbage-inflated — a
generational heap is full of dead promotions when the cadence fires — so a target derived from it
tracks the run's total promotion, its grow bracket fires on every fullsweep, and the capacity creeps
one growth-table step per fullsweep without bound for a live set that never changes.

**Workload.** A churn loop: each round allocates a fresh bounded live graph of 20 `{a, 2a}` tuples
(60 words) plus 50 garbage tuples (150 words) and then collects — 20 warm-up + 200 measured rounds,
at the crate-default `HeapPolicy` (`fullsweep_after` 65535, 233-word young / 233-word mature,
`need_reserve_words` 64). Deterministic counts only.

| | Copying (`HeapPolicy::default`) | Immix (`HeapPolicy::default`) | Immix @ `initial_mature_words` 1 024 |
|---|---|---|---|
| minors / fullsweeps | **203 / 17** | **146 / 74** | **147 / 73** |
| longest minor run | **12** | 2 | 3 |
| young capacity (constant) | 233 | 233 | 233 |
| mature capacity (constant) | **987** | **1 024** | **1 024** |
| total heap (`heap_info`) | **1 220** | **1 257** | **1 257** |

**The target arithmetic.** The live set is 60 words, the young occupancy 210, the reserve 64, so
`need_after` is **334** and the target is `next_words(2 × 334) = next_words(668) = 987` words. The
copying space installs exactly that; the mark-region space rounds the same 987-word target up to whole
256-word blocks and settles at **1 024** (and could not shrink from there, since 1 024 is inside
`[target, 4 × target]`). The cadence follows from the capacity: each minor promotes the 60-word live
set onto the mature space's top, so the occupancy climbs 60 words per minor (the copying default's
measured cycle is `60, 120, …, 780`) until the room left is smaller than the nursery's own occupancy
and the collection escalates — 12 minors, then a fullsweep.

**Bounded, not creeping.** Both capacities are a single constant across the 200 measured rounds. The
target's inputs are the live set (exact at a fullsweep) and the nursery's occupancy (bounded by the
nursery capacity, which the live-set nursery target keeps inside `[4/3 × target,
YOUNG_SHRINK_FACTOR × target]`), so the target is a **fixed point** — 987 reproduces 987, and a
clamped policy's fixed point is its clamped target — and memory does **not** grow per collection
(`tests/mature_capacity_convergence.rs` is that regression's gate).

**The cost of the multiple is mature capacity, not copy work.** On the `gc_bound/churn` shape the
second `need_after` halves the fullsweep rate — **0.034** fullsweeps/op against **0.067** at
`MATURE_TARGET_LIVE_MULTIPLE = 1`, out of **0.201** collections/op — moves the peak mature capacity
from **6 772** to **10 958** words (+4 186 words = +33 KiB) and leaves the copy work unchanged
(**271.1** words/op against **270.8** at the single multiple, `tests/churn_attribution.rs`).

**The mark-region default runs the cadence too**: at the crate default its region converges to
1 024 words (the line-rounded target), which holds the 20-line live set and leaves 44 free lines —
more than the 20 a round promotes — so it runs 2 minors per fullsweep rather than escalating every
round.

**Evidence:** `tests/generational_cadence.rs` (asserts the cadence, the `minor_gcs` advance/reset, live
term survival, reclamation and the converged constants for both strategies), `tests/steady_state.rs`
(pins young 233 / mature 987 / total 1 220 Copying; 233 / 1 024 / 1 257 Immix),
`tests/mature_capacity_convergence.rs` (the same capacities with 12 and 2 minors per fullsweep),
`tests/nursery_chain.rs`. A large off-heap loop at a 4096-word young policy converges to a nursery of
**4096** words on both strategies, mature **6 772** under Copying (4 fullsweeps) and **6 912** under
Immix (8 fullsweeps) — `tests/offheap_steady_state.rs`.

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

## The nursery chain: indexed containment and the one-step-then-doubling growth rule

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
**Growth is the one-step size unless that would not enlarge the space, then at least a doubling.** `Nursery::grow` takes `one_step = policy.next_words(need_words, 0).max(capacity)` and installs it whenever it is **larger** than the capacity it replaces; only when `one_step == capacity` (a request the space already covers, so the one-step rule would hand back the space it replaces) does it take at least `policy.next_words(2 * capacity, 0)`.
So a growth a genuinely larger request drives is **unchanged** from the plain next-size rule, and the chain of retired spaces is still `O(log(heap))` long rather than gaining one space per call.
**The two other per-reference range scans were narrowed with it.** `FromSet` keeps its ranges base-sorted
with one binary-search `locate`, and `reachable::Visited::index` delegates to it, so neither is a linear scan
over the ranges.

**The measured curve, on the route that exposed the defect.** A tiny probe module (compiled by the system
`erlc`) whose `go/0` calls `compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}])`
with `Source`/`Dir` baked into the probe source (the `run <mfa>` route takes integer args only), staged per
module and run as `erts_rust/target/debug/beam_rt run probe.beam probe:go/0`; GNU `time -v` for the wall
clock, CPU% and max RSS. Real `lib/stdlib/src/` modules. **BEFORE** is the pre-fix (linear `Nursery::contains`)
baseline measured in a sibling worktree on this host with the same probe route (`beam_gc` at `0bb28ed31c`);
**NOW** is the indexed-containment tree with the growth rule above. The shared host load differs sharply
between the two columns — `/proc/loadavg` **116–237** for BEFORE, **6–27** for NOW — so the BEFORE wall clocks
are inflated and only the shape, the retained words and the byte-identity carry across; every wall clock is an
upper bound.

| module | lines | wall BEFORE (load 116–237) | wall NOW (load 6–27, 1st / 2nd rep) | CPU% NOW | max RSS NOW (kB) | `Code` chunk (md5, unchanged?) |
|---|---|---|---|---|---|---|
| `pool` | 300 | 144.93 s | **10.01 s** | 99 % | 366 236 | 2016 B `edc120b021cc583a0908b6ddaa2989a2` **yes** |
| `ordsets` | 581 | 254.25 s | **13.99 s / 11.55 s** | 98–99 % | 334 748 / 334 664 | 1219 B `4f86664f52128d47d5eed339d27c4263` **yes** |
| `timer` | 1017 | **DNF** — `timeout 600` killed it at 600 s (RSS 469 MB, climbing) | **65.32 s** | 99 % | 747 044 | 5386 B `12f604e7d31c5e0df585026772f67647` **yes** |

(The two smallest modules the pre-fix curve also recorded — `erl_abstract_code` 8.22 s, `erl_posix_msg`
13.16 s — were not re-measured here; both the containment fix and the growth rule act on the same default
nursery policy, and the large-module shape below is the load-bearing one.)
Speedups vs BEFORE: **×14.5 / ×18.2 / >×9.2** (`timer` bounded below by the 600 s timeout).
`ordsets` is re-run for a noise band: 13.99 s and 11.55 s (the host load is shared; both reps are reported).
**Point-to-point ratios of the fixed tree's wall clock** — the scaling shape: 300→581 lines (×1.94) ⇒ **×1.40**;
581→1017 (×1.75) ⇒ **×4.67**. So the containment fix removes the term that is **quadratic** in module size
(581 lines 254 s ⇒ ~13 s, and the 1017-line case completes in ~65 s), and the small
modules are dominated by a fixed ~7 s floor.
What is **left is not linear** — the last ratio is ×4.67 for ×1.75 lines; the remaining superlinear term is
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
The containment fix **exposes** it rather than creates it: the descent is **~45 %** of a ~20 s `ordsets` run, against the **~4 %** of the section's `BEFORE` profile.
It is `beam_rt`'s to bound; nothing in `beam_gc` bounds a whole-graph walk.
Caveat (unchanged from the baseline): the perf map is empty, so JIT frames show as raw addresses in both
profiles — every frame above is native Rust, and raw addresses appear only inside the inlined closures.

**The chain's retained words (the same probe route; `minor_gcs = 0` throughout).**
The compiler's `run` process is **never collected** (`minor_gcs = 0`, `fullsweep_after` 65535), so nothing
collapses the chain, and this column is every space the run ever installed plus the (never-grown, 233-word)
mature space, read as `process_info(P, memory)` at the end of `go/0`:

| module | lines | `memory` BEFORE (pre-fix rule) | `memory` NOW (this rule) | `heap_size` NOW (words) |
|---|---|---|---|---|
| `pool` | 300 | 61 680 376 B | 112 677 312 B | 14 084 398 |
| `ordsets` | 581 | 37 899 552 B | 53 258 688 B | 6 657 070 |
| `timer` | 1017 | DNF (>600 s) | 491 376 904 B | 61 421 847 |

Read this as **what this rule's retained total is**, honestly. The retained total is the *geometric sum of the
whole ladder* the compile installs, so it is `O(the largest space the compile forces)` and never a function of
the iteration count — but its multiplier is the ladder's, not a constant 2.
This rule installs a **one-step** rung wherever the request is only just above the space (growth-table ratio
≈ ×1.618, so its chain sums to ≈ ×2.618 of the final rung) and a **doubling** rung only when the request
already fits the space it replaces (ratio ≈ ×2.618, summing to ≈ ×1.618 of a coarser rung).
So a finer ladder lands on a **lower final rung** (a smaller workload hands back a smaller space) while its
longer geometric tail sums to a larger multiple; which dominates is workload-dependent — measured here as
**more** than the pre-fix one-step rule for all three (`pool` 112.7 M vs 61.7 M B; `ordsets` 53.3 M vs 37.9 M B;
`timer` 491.4 M B, no pre-fix figure).
The peak RSS is unchanged within noise (335 / 366 / 747 MB — the ~300 MB VM/arena baseline dominates) and
**nothing collects on this route**, so the retained total is bounded only by the requirement, never by the
iteration count.

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

| quantity the 400-cycle corpus pins | peak, Copying | peak, Immix | bound asserted |
|---|---|---|---|
| chain length (spaces) | **2** | **2** | `peak_spaces == 2` — one retired space at most |
| retention (retired capacity words) | **15 143** | **8 980** | `peak_retained <= peak_young`, `tail_retained <= tail_young` |
| current nursery (words) | **46 422** | **37 670** | `peak_young <= next_words(2 × 8 × young_target_words(max_live))` (against the 400 × 256 = 102 400 an iteration-linear leak shows) |
| converged tail (last 20 states) | 1 space, **0** retained words | 1 space, **0** retained words | `tail_spaces <= 2`, at most 12 distinct tail states |
| mature capacity | **17 731**, constant over the tail | **4 352**, constant over the tail | one constant over the whole tail, on both strategies |

**The peak nursery is a function of the live set, never of the iteration count.** Each round's 256-word
tuple is promoted by the next minor and stays live until the following fullsweep, so the run's largest
live set is 1 792 words (Copying) / 2 560 (Immix) and `young_target_words` answers 4 185 / 6 772 — which
is why the peak capacity is tens of thousands of words (a settled capacity keeps up to
`YOUNG_SHRINK_FACTOR` × its target, and a growth adds at most one growth-table step or one doubling),
while the same figure would be 102 400 words for a leak that grew per iteration.
**The chain's LENGTH is the bounded quantity, not a per-call step.** `grow_nursery` takes the one-step size
when it enlarges the space and at least a **doubling** when it would not (the mechanism above), so a growth
loop repeating a **constant** request still climbs a geometric ladder: the chain is `O(log(heap))` long and
the retained words are the ladder's geometric sum, never a function of the iteration count. Only a
collection's `collapse` frees a retired space, and it collapses the chain to **exactly one** space.
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

**Mark-region contiguity: promotion is ATOMIC and the collector RECOVERS.** A **100-word** (7-line) tuple at a 2048-word region is the shape a non-atomic promotion cannot place: `ImmixSpace::alloc_words` needs a run of the object's 7 lines **inside one 16-line block** (`find_run`/`first_hole`), while a promote/reclaim pre-check against the region-wide `free_lines()` sees free lines no single run can use (two 7-line objects fill 14 of a block's 16 lines).
A promotion that shares the `forward_slot` marker protocol with the rewrite that follows it fails **mid-copy**, surfacing as a spurious allocation error (`Alloc(AllocFailure { requested: 800, kind: Heap })`) within **43 rounds** of the repeating cycle while `mature_capacity_words()` still reports 2048.

Both halves are closed, deterministically:

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
  refusal as well as on `free_lines() < needed`; and `grow_major` also rebuilds a **fresh region of the
  current capacity** when a `max_heap_words` ceiling forbids growing — an empty region packs monotonically,
  so the run the fragmented in-place region cannot produce is exactly what it has. So the failure is
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
**What the probe runs.** `tools/bench`'s own `copy_bound` loop body, on the **same A/B binary pair** as the sections
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

## Per-op attribution of the `gc_bound/churn` shape (the collector's own counters)

Source: `tests/churn_attribution.rs` (`--nocapture`; deterministic counts only, no timing asserted). It
reproduces the `tools/bench` `churn/1` allocation shape — LEN = 256 (a 512-word list built one cons cell at a
time, a 256-byte binary copy-out, a 257-word tuple, two O(1) checks ⇒ **806 words allocated/op**, all of it
garbage by the next op) — and drives it through the runtime's own heap-admission door
(`Collector::should_collect` / `collection_debt` → `Collector::ensure_capacity`, i.e. `beam_rt`'s
`collect_or_grow`) at the **default spawn policy**, with the op's intermediates held as roots.
The second column is the real `beam_rt` runner on its own `churn.beam`, re-derived by
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib
runner::tests::the_churn_workloads_per_call_counters_are_flat_in_n -- --nocapture`.

| per operation | in-crate instrument (4 000 ops + 500 warm-up) | `beam_rt` runner counters, same shape (`churn.beam`, N = 40 000) |
|---|---|---|
| environment | `beam_gc` unit-shaped driver, `HeapPolicy::default()` | real runtime, scheduled slice |
| words allocated | 806 (asserted from the shape) | ≈1 830 (3 × 256 cons + copy-out + tuple) |
| collections | **0.201** (0.167 minor / 0.034 major) | **1.734** (69 352 collections, 3 347 of them fullsweeps) |
| words copied | **271.1** (0.34 × the allocated volume) | — |
| words reclaimed (junk) | 806.0 (≈ all of it) | — |
| objects copied | 103.40 | — |
| words copied / collection | 1 350.8 | — |
| growth events (space installs) | 0.0007 | — |
| debt admissions | 0.0000 | 0 (no boundary holds a retired space) |
| peak nursery capacity (occupancy) | 4 185 (3 541 words) | 1 220 |
| peak mature capacity / occupancy | 10 958 / 8 118 words | 4 185 / 4 068 words |
| fullsweep live set (min…peak) | 546 … 1 353 words | — |

**Reading the split.** Because the nursery is sized from the live set, the copy volume sits **below** the mutator's allocation volume — 271 copied words per 806 allocated, i.e. **0.34×** — as the `live / nursery` ratio predicts: the nursery holds several allocation rounds before the (fixed-cost) live set is evacuated again, with `YOUNG_TARGET_LIVE_MULTIPLE` (2) the floor of that ratio.
The collection count is the small term (one per five ops) and the copy loop over the live set is what an op pays per collection, so the per-op cost is one collection's copy work rather than a multiple of it.

**Where the few escalations come from (mechanism, `src/collector.rs`).** `Collector::collect` escalates a
minor to a fullsweep whenever `mature.available_words() < nursery.used_words()` — the test is against the
nursery's whole **occupancy**, not the live young set a minor would actually copy. The instrument records the
escalation's inputs: the mature had as little as **1** free word when an escalation fired, against a nursery
occupancy of up to **4 035**, so every escalation is a genuine "the mature is full of promoted
garbage", not a conservative-test artifact. `fullsweep_after` (65 535) never fires: all 0.034
majors/op are **capacity**-forced, and the fresh mature space is sized by
`mature_after_fullsweep(capacity, live, young_occupancy)`, which on this shape takes the grow branch or
keeps the capacity — it never shrinks.

**Bounded-growth sanity (asserted).** The nursery stays at a policy size (peak 4 185 words, `≤ 16 Ki`
asserted), the mature at ≤ 64 Ki words asserted, and the chain never holds a retired space at all
(`peak_spaces == 1`) — so nothing here is capacity creep.

**Caveats.** In-crate numbers come from a debug-profile test binary on a shared host; they are counts, so
they are load-invariant, but the *shape* is a model (the copy-out binary is a 33-word tuple, and the
driver's doors sit on the op's own allocation boundaries rather than on the interpreter's `test_heap`
instructions), which is why the runtime column differs in magnitude while agreeing on the structure: a
collection whenever the settled nursery fills, a fullsweep only where the mature generation fills, and
peak capacities driven by the shape's own live set. The runtime column's per-call ratios are flat in N
— **1.7331 / 1.7339 / 1.7338** collections per call at N = 2 000 / 10 000 / 40 000, with
**0.4523 / 0.4525 / 0.4525** slices per call — so it is a per-call property, not a startup offset.
The canonical table and the protocol are `beam_rt`'s
(`scripts/cargo.sh test -p beam_rt --lib runner::tests::the_churn_workloads_per_call_counters_are_flat_in_n
-- --nocapture`).

## The nursery follows the live set — the live-set nursery target

Source: `tests/churn_attribution.rs` (deterministic counters, load-invariant) plus the paired
`beam_rt` A/B below.

**The mechanism.** The nursery's post-collection capacity is ONE live-set target per collection
kind, never a function of the nursery's own capacity or of its pre-collection fill.

* A **fullsweep** measures the **whole-heap** live set exactly (it copies every live word), so it
  sizes the nursery from `young_target_words(live) = next_words(YOUNG_TARGET_LIVE_MULTIPLE * live +
  need_reserve_words, 0)`, i.e. `next_words(2 * live + 64)`, through `Nursery::collapse_after_fullsweep`
  → `young_after_fullsweep(capacity, live)`: grow to the target, shrink back to it once the live set
  has fallen `YOUNG_SHRINK_FACTOR` (8) times below it, else keep.
* A **minor** measures only the **young** live set — the words it evacuated, `GcStats::words_copied`
  (`GenImmixCollector::run_minor` feeds the promoted words, the same exact quantity) — and sizes the
  nursery from `young_minor_target_words(live) = next_words(live + need_reserve_words, 0)`, i.e.
  `next_words(live + 64)`, through `Nursery::collapse` → `young_after_collection(capacity, live)`: a
  three-way choice that **installs the target** when the capacity is above `YOUNG_SHRINK_FACTOR` × the
  target (the live set fell far below what the nursery is sized for: give the memory back) or below it
  (enlarge onto the target), and otherwise **keeps** the capacity. A minor therefore both grows and
  shrinks, always onto a capacity derived from the live set.
  The mature space's occupancy is **never** a minor's live figure: a minor does not copy the mature
  generation (only a fullsweep does), so that space carries every garbage object the minors since the
  last fullsweep promoted, and a target derived from it tracks the run's *total promotion* instead of
  its live set.

Two ratios fix the constants: a copying nursery amortizes the live set's evacuation over the words
the mutator allocates into it, so `live / nursery` is the collector's copy work per allocated word and
the fullsweep's `YOUNG_TARGET_LIVE_MULTIPLE = 2` halves it (the minor's figure is young-only, an
under-estimate of the heap's whole live set, so it takes one times the live set — the smallest curated
capacity at which the round's own survival does not itself force the next collection); and the band
from `YOUNG_SHRINK_FACTOR = 8` is wider than the ~3× a live set swings inside one operation, so the
capacity is a fixed point rather than an oscillation. Both targets run through the monotone
`next_words`, so neither can land below `nursery_floor_words() = next_words(need_reserve_words, 0)`
(**90** words for the default policy, **233** = `DEFAULT_MIN_HEAP_WORDS` for a spawn policy — the
figure `beam_rt`'s `COLLECTOR_FLOOR` pin reads), and `young_minor_target_words(0)` *is* that floor.
The C reaches the same decision in two steps — `major_collection` sizes the receiving young heap from
the pre-copy occupancy (`erl_gc.c:1866`), then `adjust_after_fullsweep` re-sizes from the post-copy
live set (`erl_gc.c:2062-2098`) — because a C young heap is only *part* of the process heap. Here the
nursery is its own space, so its occupancy is bounded by its own capacity
(`used_words() <= capacity_words()`) and can never signal demand beyond the live set: the pre-copy
step collapses into the live-set decision.

| per operation (4 000 ops + 500 warm-up) | capacity-keyed rule (`5ddf6e72e4`) | live-set target (current) |
|---|---|---|
| words allocated (shape) | 806 | 806 |
| collections | 1.500 (1.000 minor / 0.500 major) | **0.201** (0.167 / 0.034) |
| words copied | 1 283.5 | **271.1** |
| words reclaimed (junk) | 806.6 | 806.0 |
| objects copied | 425.01 | **103.40** |
| words copied / collection | 855.4 | 1 350.8 |
| growth events (space installs) | 0.0004 | 0.0007 |
| peak nursery spaces | 1 | 1 |
| peak nursery capacity / occupancy | 609 / 461 | **4 185** / 3 541 |
| peak mature capacity / occupancy | 2 586 / 1 960 | **10 958** / 8 118 |
| fullsweep live set (min…peak) | 464 … 1 353 | 546 … 1 353 |
| peak resident capacity (nursery + mature) | 3 195 | **15 143** |
| peak resident occupancy (nursery + mature) | 2 421 | **11 659** |
| escalation: mature room min vs nursery occupancy max | 1 vs 550 | 1 vs 4 035 |

**7.5× fewer collections (1.500 → 0.201/op) for 4.7× less copy work (1 283.5 → 271.1 words/op) and
4.1× fewer objects copied at the cost of resident words** (the two capacities sum to 3 195 → 15 143,
i.e. 4.7×). That is the trade the `live / nursery` ratio predicts: a nursery of `k` × the live set
divides the copy work by `k` and costs `k` × the live set in resident words, and the churn's live set
(546 … 1 353 words) is what both figures follow, while the mature generation spends its own
`MATURE_TARGET_LIVE_MULTIPLE` on amortising the fullsweeps (see "The mature-generation room policy").

**Boundedness (asserted, not hoped).** Every branch of either sizing rule returns either the current
capacity or a target that depends **only on the live set** — so re-deriving it at the same live set
reproduces it: `young_after_fullsweep(target, live) == target` and
`young_after_collection(young_minor_target_words(live), live) == young_minor_target_words(live)` for
every `live`, exhaustively over the growth table and over min/max-clamped policies (`src/policy.rs`'s
in-module suite). The nursery therefore never exceeds `YOUNG_SHRINK_FACTOR` × its target, a burst that
outlived its data is corrected at the next fullsweep, and a nursery grown for one request comes back
down: a burst nursery of 2 819 words returns to 233 once its live set is 60 words,
`young_after_fullsweep(100_000, 0)` is 90 words, and a grossly oversized nursery collapses to 90 on
**both** strategies (`tests/heap_sizing.rs`). `tests/nursery_chain.rs`'s grow-then-collect loop
converges to **exactly one** state — `(1 space, 1 598, 1 598)`, the live set's own target and a fixed
point of the mature rule — instead of alternating between a grown and a collapsed nursery. The three
branches (shrink to the target, grow to it, keep inside the band), the churn geometry, the fixed point,
`a_minor_may_grow_only_onto_the_live_set_target` and the two constants are pinned one test each by
`tests/post_fullsweep_nursery.rs`.

**A/B of the two runtimes on the same shape.** Release `beam_rt`,
`bench run --quick --runtime rust --workloads gc_bound`, pinned to one core, five alternating paired
rounds, loadavg 13–22: base (`5ddf6e72e4`) **71 083.3 ns/op**, the live-set target (`49a8df25ad`)
**63 791.1 ns/op**, per-round improvement **1.152 / 1.096 / 1.138 / 1.058 / 1.170×**,
**median 1.138× (spread 1.058–1.170×)**; peak RSS **119.5 → 122.8 MiB** (+2.8 MiB). Against the system
oracle in the same window (median **4 585.9 ns/op**, N = 200 000) the cross-runtime ratio moves
**15.50× → 13.91×**.

**The residual is not collection work.** Removing 1.3 of the 1.5 collections/op saves 7.3 µs of a 71.1 µs/op op, so the remaining ≈63.8 µs/op is the interpreted loop, the per-heap-admission root walk and `beam_bifs`' allocation churn — the attribution in `../../BOTTLENECKS.md` §B1's `perf` profile (copy loop 4.2 % of samples, interpreter 20 %, libc 15 %, `beam_rt::context::gc` 12.7 %, `beam_bifs` `Vec` 4.9 %).

**Caveats.** The counter table is the evidence; the host is shared (loadavg 13–105 across these runs), so
the A/B is quoted only as the paired per-round ratio and its median, never as an absolute wall time.
The shapes whose geometry a policy floor or an explicit `initial_mature_words` fixes are unchanged
counter-for-counter: `tests/allocation_heavy_workload.rs` (5 334 minors / 2 666 majors / 19 115 520
words moved / 14 848 peak resident / steady 32 768–32 768) and the Immix corpora (`immix_workloads` /
`immix_measure` / `immix_parity` / `immix_stress` / `immix_break_even`).
The shapes that run at the literal default policy follow the mature target:
`tests/generational_cadence.rs` (Copying **203** minors : **17** fullsweeps at young 233 / mature 987 /
live 60; Immix 146 : 74 at 233 / 1 024, and 147 : 73 at `initial_mature_words` 1 024),
`tests/mature_capacity_convergence.rs` (233 / 987 and 233 / 1 024), `tests/retained_vs_live.rs`
(8 192 young / 17 731 mature / 25 923 total, Copying; 8 192 / 17 920 / 26 112, Immix) and
`tests/memory_overhead.rs` (live 2 304 words, nursery capacity 6 772, mature 10 958, RSS plateau ratio
**1.000×** — a plateau, not a leak). Only the churn shape is A/B'd end to end; everywhere else the
figures are the crate's own suites' counters.

### The acceptance measurements: a collection's capacity is the live set

Three independent probes, all on this tree. The fixtures are `beam_rt`'s
(`tests/fixtures/{reds_boundary,native_reds}.beam`): a BIF-only loop that builds and abandons a
400-element tuple per iteration, so its live set is either ≈ 0 words or the 401 words of the one
in-flight tuple and does not grow with N. Every figure is a count or a capacity — no timing.

**1. The boundary-door pin (`beam_rt`'s `reds_door`, interpreter tier, collected at the
reduction/yield boundary between slices).** `heap_size` is **987 words at N = 2 000, 20 000 and
200 000**, with `peak_young_spaces = 1` and **9 / 89 / 899** collections (one per pre-emption) —
flat in N, and the same capacity at every point of a 100× range.
987 = `young_target_words(401)` = `next_words(2 × 401 + 64)`, the **fullsweep** rule's target for
the live set the boundary window actually holds: the first boundary's kind-pinned minor cannot be
promoted into the 233-word initial mature region, so it escalates and the fullsweep installs that
target, which every later minor keeps because 987 sits inside
`[young_minor_target_words(401) = 610, 8 × 610 = 4 880]` — 610 is the band's lower edge, not the
converged capacity.

**2. The interpreter control of the slice-charging pin (`beam_rt`'s `slice_charging`).** At
N = 2 000 it reports `heap_size = 233` — the process's `min_heap_words`, i.e.
`young_minor_target_words(0)` = `nursery_floor_words()` for a spawn policy. Every
boundary-sampled row reads **233 for every N** measured — 799, 800, 2 000, 4 000 and 20 000
(`value=233` on each, one boundary collection each).

**3. A per-minor trace over `reds_boundary:run/1`** (a temporary per-minor diagnostic; the figures
are the collector's own per-collection counters). Every boundary minor evacuates
`young_live_copied = 401` (once in four — the iteration's in-flight tuple is the live set) or `0`,
which gives `minor_target = 610` / `233` against `floor = 233`: the capacity a collection installs
is the live set's target, both branches of the same rule. The figure a minor does *not* read — the
mature space's occupancy — crept 4 411 → 8 822 over that same window, and its 2× whole-heap target
10 958 → 17 731: a capacity that follows the run's total promotion rather than its live set.

**The churn shape's own counters** (`tests/churn_attribution.rs`, 4 000 ops + 500 warm-up): 0.201
collections/op (0.167 minor + 0.034 major), 271.1 words copied/op, 103.40 objects copied/op, 806.0
words reclaimed/op, 0.0007 growth events/op, peak nursery spaces 1, peak nursery capacity 4 185 words
(occupancy 3 541), peak mature capacity 10 958 (occupancy 8 118) → peak resident occupancy 11 659 words
against a peak resident capacity of 15 143, fullsweep live set 546 / 1 353. The table above is that run.

### The flatness-vs-amortisation trade

A minor's target is **one** times the young live set, so the nursery holds `target / per-iteration
allocation` iterations of the mutator's own allocation and the capacity stays **flat in N** at a
constant live set.
A larger minor multiple would amortise the copy work further; it is excluded because the amortisation
would price memory on a *young-only* under-estimate of the live set and would move the converged young
capacity off the figure the dependents pin.
The measured case for that choice, all four probes on this tree:

**1. The dependent runtime's constant-live-set flatness probe.** The owner is `beam_rt`: a probe that
rebuilds a 400-element tuple every iteration and samples `process_info(self(), heap_size)` at **every**
iteration, run as a release build of the runtime at four N values spanning 2 000 … 200 000 (a 100×
range). Over those samples the **min** `heap_size` is **987** and the **max** **830 667** on the native
baseline-JIT tier, and **2 586** / **134 530** with `BEAM_RT_TIER=interpreter`; the post-loop reading
is the collector's floor, **233** = `young_minor_target_words(0)`, wherever the run ends on a
collected row (`slice_charging`'s N = 2 000 control, **233** on both tiers; a run that ends inside an
uncollected burst reads that burst's chain sum instead — see "The CLI flatness probe and the burst
residual"). Both ends are flat in N — the min is
the capacity the policy installs, the max is the **chain-summed** capacity mid-burst (`Nursery::grow`
retires spaces without moving terms), so the max is a burst-cycle fixed point rather than a leak. A
single sample is not a measurement: one arbitrary iteration of the same run reports anything from
**2 586** to **830 667** (a 321× spread across sample points), because a sample lands mid-burst on the
garbage the iteration has just allocated. The canonical table and the probe are `beam_rt`'s.

**2. The same trade in the crate's own counters** — the `gc_bound/churn` attribution above: **0.201**
collections/op = **0.167** minor + **0.034** major, **271.1** words copied/op (0.336× the 806 words the
shape allocates per op), **103.40** objects copied/op, peak nursery **4 185** capacity / 3 541
occupancy, peak mature **10 958** / 8 118, peak resident **15 143** words, fullsweep live set min/peak
546 / 1 353, end state nursery 4 185 / 0 and mature 10 958 / 4 059.

**3. The dependent runtime fixture that reads the same ratio at a much larger allocation per
iteration** — `beam_rt`'s `boundary_cost` (release, read-only from here), re-measured on this tree.
Its N = 2 000 row reads
`collections=4728 major=325 door_checks=904 door_collections=105 peak_young_spaces=1
peak_young_capacity=2585 peak_mature_capacity=6772 peak_mature_used=6712`. That is **2.36
collections per call** for a **2 585**-word nursery carrying **3.2** iterations of allocation
(2 585 / 806) and a **6 772**-word mature region, its boundary door firing **one collection per
8.61 checks** (`904 / 105`, inside the 7..9 watermark band its owner pins) at
`904 / 2 001 = 0.452` checks per call. Both capacities are the live-set rules' fixed points, not
policy captions: the nursery is a **chain sum materialised into a single young space** by
`young_after_collection`'s keep branch — the growth seam had taken the chain to
`987 + 1598 = 2 585` words, inside the keep band around the fixture's 376-word minor target
(`next_words(201 + 64)`), so the collapse kept the figure it was handed and installed it as one
space (2 585 is not a growth-table entry; 2 586 is) — and the mature region is
`next_words(2 × (201 + 2 585 + 64)) = next_words(5 700) = 6 772`; its owner's `MEASURED` table
(`crates/beam_rt/src/runner/tests/boundary_cost.rs`) re-derives both from every row.

**4. The suite those probes live in** is `beam_rt`'s: 1 805 passed / 1 failed / 4 ignored when this
section was written, the single failure being that fixture's own stale `MEASURED` table — its owner has
since re-derived it, and `the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape`
passes on this tree (row quoted above). The two `beam_gc` pins inside it both pass — `reds_door`
(converged `heap_size` **987** at N = 2 000, 20 000 and 200 000) and `slice_charging` (interpreter
control **233**).

### The CLI flatness probe and the burst residual

`erts_rust/target/debug/beam_rt run erts_rust/crates/beam_rt/tests/fixtures/native_reds.beam
native_reds:heap/1 <N>` — the process's own final `process_info(self(), heap_size)` reading, on the
default (native baseline-JIT) tier and with `BEAM_RT_TIER=interpreter`. "before" is the same probe on
the pre-change tree.

| N | native, after | native, before | interpreter, after | interpreter, before |
|---|---|---|---|---|
| 2 000 | 233 | 233 | 233 | 2 586 |
| 20 000 | 233 | 233 | 309 923 | 503 876 |
| 100 000 | 233 | 233 | 297 368 | 300 453 |
| 200 000 | 233 | 233 | 280 628 | 300 453 |

**The native tier** runs the whole loop inside one slice, so it reaches no collection at all and
stays on the floor — 233 words, flat in N.

**The interpreter tier** is pre-empted (~89 slices at N = 20 000), so between two boundary
collections it allocates the whole slice's worth of garbage — ~266 000 words at N = 20 000 — with no
collection possible in between. The capacity **at a collection** is the live-set figure (233 / 987 —
the floor and the 401-word live set's fullsweep target, measured above and flat at every N); the
process's own *final* reading is a **mid-burst** sample of
a nursery chain whose length tracks the burst's allocation count, so it is level in N but not
identical across N. It is the slice's allocation demand, not a nursery-policy quantity, and the next
collection collapses the chain back onto the live-set target.

**The residual trade-off, stated plainly.** The growth seam's installed size is capped by the
remembered live target (`Nursery::grow`'s `min(target, max(one_step, live_target))`), so a burst
whose in-slice demand exceeds that target installs a **chain of target-sized spaces** (the summed
capacity ≈ the demand) instead of climbing the growth table geometrically. The retained **words** are
the burst's own allocation (capacity ≈ occupancy + one space); the cost is the **space count**
(~500 spaces of 610 words at N = 20 000) and a final reading that depends on where in the burst it
falls. A collection collapses the chain to one space, so nothing is retained across collection
cycles.

### The rejected nursery-sizing variants

Three other ways to size the nursery were measured and rejected. Each fails a property the live-set
target has, and each stays a **live regression gate** in `tests/` rather than a historical note.
* **Capacity-keyed (the C shape).** Size from the pre-copy fill, then shrink by `2 * need_after` after
  the copy. Its growth branch requires `demand > capacity`, and the inputs it can see are ≈ the
  capacity or below it — a minor's promoted words are its young live set, which a nursery sized at that
  live set's own target already holds, and a fullsweep's pre-copy fill is ≈ the capacity — so the
  branch does not fire and the nursery can only be enlarged by an explicit request. Measured: it
  settles at a 609-word nursery against a 464–1 353-word live set, i.e. a copy volume of 1.58 × the
  allocated volume (the `5ddf6e72e4` column above).
* **Keyed on the nursery's own fill or on its capacity.** The fill is ≈ the capacity at every collection,
  so `next_words` steps the growth table up on *every* collection and the capacity ratchets: a 200-round
  loop grows the nursery ~20 % per round with no bound, and RSS scales with the workload (the unbounded
  `foreign` regime in *Heap-sizing measurements*). `tests/nursery_bounded.rs` is that regression's gate.
* **An offset-1 step, `next_words(capacity.max(request), 1)`.** The same defect from the other side: it
  fires on every collection whatever the live set is (`tests/nursery_bounded.rs`).
### The cadence against the oracle (counts)

Source: a `$TMPDIR`-built `gcprobe6.erl` (re-created there from the shape below — its source lives only under `$TMPDIR`, never in the repo), both sides run on the same host with the same probe: `beam_rt [flags] run gcprobe6.beam gcprobe6:{steady,live}/1 <N>` against `erl -noshell -pa $TMPDIR/gccheck -eval "gcprobe6:{steady,live}(<N>)" -s init stop`; sys = OTP 29.1.1 / erts-17.1, rust = `beam_rt` at its default baseline-JIT tier (both debug builds).
**Every figure is a count, so it is load-invariant; no timing is quoted** (`/proc/loadavg` 0.5–3.2, `nproc` 14), and a second identical run reproduces every cell.
**The shapes.** Both keep a list alive and iterate a `_ = [{I, I*2, [I], erlang:phash2({I, I})} || I <- L]` comprehension over it per iteration — a fresh 4-element tuple plus a one-element list per element, all of it garbage by the next iteration.
`steady/1` keeps a 200-element list, `live/1` a 2 000-element one (the live-set-protected regime).
**`collections` is the loop's own delta of `erlang:statistics(garbage_collection)` element 1** (minors + majors), so no start-up collection of either side is counted.
**`minor_gcs` is the C `GEN_GCS` on both sides — minors since the last fullsweep, reset to 0 by a fullsweep** (`erl_gc.c:1511`/`:1943`) — never a lifetime total, so a small value does not mean few collections.
`collections - minor_gcs` is the run's fullsweep count: **2** on every `steady/1` row, and on `live/1` **2 / 65 539 / 196 612** — `fullsweep_after`'s cap of 65 535 minors is crossed once by N = 3 000 and three times by N = 10 000.
**A minor's nursery target is one times the young live set the round evacuated, and that geometry is the cadence.** `young_minor_target_words` sizes it from `live + reserve`, so once the kept list has been promoted to the mature generation the young live set is a few hundred words and the final sample reads a 466-word chain on both shapes (376 + 90); the rust process therefore collects **more often** than the oracle, **2.50×** its per-iteration count on `steady/1` and **18.0×** on `live/1`, at a nursery of 466–699 words against the oracle's 610 / 4 185.

| `steady/1` N | oracle `minor_gcs` | rust collections | rust `minor_gcs` | rust `heap_size` / `total_heap_size` | oracle `heap_size` |
|---|---|---|---|---|---|
| 1 000 | 1 050 | 2 593 | 2 591 | 466 / 7 471 | 610 |
| 3 000 | 3 144 | 7 824 | 7 822 | 699 / 7 471 | 610 |
| 10 000 | 10 474 | 26 134 | 26 132 | 466 / 7 471 | 610 |

| `live/1` N | oracle `minor_gcs` | rust collections | rust `minor_gcs` | rust `heap_size` / `total_heap_size` | oracle `heap_size` |
|---|---|---|---|---|---|
| 1 000 | 1 448 | 25 886 | 25 884 | 466 / 46 888 | 4 185 |
| 3 000 | 4 340 | 77 871 | 12 332 | 466 / 46 888 | 4 185 |
| 10 000 | 14 463 | 260 007 | 63 395 | 466 / 29 389 | 4 185 |

**Rust `heap_size` is the nursery chain's summed capacity at the process's final sample**, i.e. a mid-burst reading of the chain the growth seam installs — 466 = 376 + 90, 699 = 376 + 233 + 90 — so it is level in N rather than identical across N (the figure a *collection* installs is the live set's target, pinned below and by `../beam_rt`'s own probes).
**`total_heap_size` is the mature capacity the target-keyed fullsweep rule installs** (`mature_target_words` = `MATURE_TARGET_LIVE_MULTIPLE` × the live set plus the young occupancy plus the reserve): 7 471 words on `steady/1` and 46 888 on `live/1` at N = 1 000 and 3 000, falling to 29 389 at N = 10 000 because the `fullsweep_after`-forced fullsweep near the end of that run resizes the mature space onto its target.

**Slopes (per loop iteration, N = 1 000 → 10 000).** `steady/1`: oracle **1.047** `/op`, rust **2.616** `/op` — **2.50×** the oracle's; `live/1`: oracle **1.446** `/op`, rust **26.01** `/op` — **18.0×**.
Both sides are linear in N, so the ratio is a per-op property, and the two slopes differ with the two heaps' geometry: the kept list rides the mature heap and the nursery is a 466-word chain against the oracle's 610 / 4 185, paid for in footprint (`total_heap_size` 5.75× / 5.54× the oracle's at N = 10 000; node `erlang:memory(total)` **7.8–8.0 MB** against **34.9 MB**, where the oracle's total carries a far larger process population).
**The heap-size flag is the cadence lever.** A `+hms` above the live-set floor dominates the rule and cuts the count on both shapes, monotonically across these three floors, because what predicts the count is the nursery the flag helps install: 233 → 1 000 → 20 000 divides the rust count by ~7 and then ~17 on `steady/1`, and by ~7 and then ~17 on `live/1`.
Rust, N = 10 000, collections (`heap_size`):

| shape | `+hms` 233 | `+hms` 1 000 | `+hms` 20 000 |
|---|---|---|---|
| `steady/1` | 26 134 (466) | 3 772 (1 598) | **223** (28 690) |
| `live/1` | 260 007 (466) | 38 035 (1 598) | **2 225** (28 690) |

**The regime difference is NOT `DEFAULT_HEAP_WORDS`.** `beam_rt`'s 16.7 M-word default (`../beam_rt/src/config.rs`) sizes the runtime's term-construction heaps (BIF-heap pool, `-eval` heap), never a process nursery: a fresh `run` process reports `heap_size` **233** words against the oracle's **376** (both with `total_heap_size` equal to the same figure), and the nursery is the heap-size policy's from then on.
`+hms 233` (the oracle's own `H_DEFAULT_SIZE`) reproduces the default column **exactly**, on both shapes.
**The probe holds no foreign BIF container across the loop.** A `statistics(garbage_collection)` tuple bound before the loop and kept live measures the same counts as the integer baseline (`steady/1` N = 1 000: rust **2 593**, oracle **1 042** against **2 593** / **1 048** unheld), so neither side's cadence here is an unscanned-root-valve artifact.
**Load-bearing.** The two halves are pinned separately in `tests/post_fullsweep_nursery.rs`: the fullsweep's growth/shrink/keep branches against `young_target_words(live)` at a fixed live set, and `a_minor_may_grow_only_onto_the_live_set_target` for the minor half — a minor installs `young_minor_target_words(live)` (its own young live set), so it grows as well as shrinks, and never onto a capacity derived from the mature occupancy.
A target derived from the capacity or the fill instead is that rule's regression, pinned by `tests/nursery_bounded.rs`.
## The off-heap binary roster: reachability reclaim and the binary-virtual-heap trigger

Sources: `tests/offheap_retention.rs` (the in-crate ladder and the without-sweep contrast — all
deterministic counters, asserted in the test) and an end-to-end probe of the real `beam_rt` binary on a
purpose-built `.beam` (below). The ladder runs from the repository root with

```
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc \
    --test offheap_retention -- --nocapture
```

**What is measured.** The two seams of this crate's off-heap half. The **reclaim** is
`src/offheap.rs`'s `sweep_unreachable_binaries` — the C `sweep_off_heap`: it drops every
`OffHeapEntry::Binary` that no reachable `BinRef` names, always keeps the `Mso` entries, and is
slot-stable through `OffHeap::sweep`. The **trigger** is `src/bin_vheap.rs`'s `BinVheap`: the
surviving overhead's word count against `limit_words`, i.e. the C `ERTS_IS_GC_DESIRED_INTERNAL`
`off_heap.overhead > bin_vheap_sz` clause, with `record_collection` re-seeding the limit through the
C `do_next_vheap_size` rule (grow at ≈1.618, shrink by half, floored by
`HeapPolicy::min_bin_vheap_words`). The ladder isolates the reclaim by building a fresh collector per
N; the runtime probe below (taken before the runtime drove the pair) shows what the **missing
trigger** costs when the roster's owner does not drive it.

### The in-crate ladder — flat in N

The workload allocates one `RefcBinary::with_capacity(65_536)` payload per call, roots the frozen
`[BinRef][ErlSubBits]` handle pair naming it, registers the owning handle in the roster, and — when
`BinVheap::triggered_for` fires — collects, sweeps the roster and re-seeds the limit from what
survived; a final collection and sweep settle the ladder, and every figure below is that settled
state. One retained payload reports **262 192 B** of `binary_alloc` `live_bytes` because the
reservation is **4×** the requested capacity, and the carrier is one 1 MiB block. The policy is the
crate default `min_heap_words` (233) with `max_heap_words` = `1 << 16` as a real ceiling and
`min_bin_vheap_words` at its default floor.

| N | `binary` live | carrier | roster len (peak) | released | collections | heap | debt |
|---|---|---|---|---|---|---|---|
| 0 | 0 B | 0 B | 0 (0) | 0 | 1 | 233 w, 1 space | false |
| 250 | 262 192 B | 1 048 576 B | 1 (6) | 249 | 50 | 233 w, 1 space | false |
| 500 | 262 192 B | 1 048 576 B | 1 (6) | 499 | 100 | 233 w, 1 space | false |
| 1 000 | 262 192 B | 1 048 576 B | 1 (6) | 999 | 200 | 233 w, 1 space | false |

**Flat in N.** Live bytes, carrier bytes, roster length and the roster's peak are *identical* at
N = 500 and N = 1 000: the roster holds exactly the payload the loop is still using, whatever N is,
while the collection count carries the loop (≈one per five calls, the settling collection included).
The heap never leaves the 233-word `H_DEFAULT_SIZE` floor with a single space and `debt` stays
`false`, so none of the shape's cost is the collector's footprint. Over the whole ladder the
process's `VmHWM` moves **3 600 → 4 296 KiB**.

**Without the reachability sweep** — the owner keeping its own roster and collecting only its heap —
the same loop at N = 250:

| measure | with `sweep_unreachable_binaries` (N = 250) | roster untouched (N = 250) |
|---|---|---|
| `binary` live | 262 192 B | **65 548 000 B** |
| carrier | 1 048 576 B | **88 080 384 B** |
| roster len (peak) | 1 (6) | 250 (250) |
| released | 249 | 0 |

i.e. **one retained 64 KiB payload per call, ≈64 KiB/call** of allocator live bytes grown
monotonically. The roster is a leak unless something reclaims it, and a heap collection alone cannot
(the runtime probe below).

### End-to-end probe of the real `beam_rt` binary

A purpose-built module (`expand/1`: one `bs_init_writable` plus 16 384 `private_append`s per call,
64 KiB out) run as `beam_rt run expand.beam expand:expand/1 N`, reporting `erlang:memory(binary)`,
`process_info(self(), binary)` and `erlang:memory(processes)`; peak RSS from `/proc/<pid>/status`
`VmHWM`. The build deliberately stays below the on-heap limit so every call materializes off-heap.

| N | `erlang:memory(binary)` | off-heap roster len | `erlang:memory(processes)` | peak RSS |
|---|---|---|---|---|
| 0 | 16 432 B | 0 | 445 904 B | — |
| 200 | 13 133 232 B | 201 | 1 416 328 B | 129 432 KiB |
| 800 | 52 483 632 B | 801 | 1 416 328 B | 168 548 KiB |

**One payload per call.** The roster is `N + 1` (the live payload plus one retained per call), and
the N = 200 → N = 800 delta is 39 350 400 B over 600 calls — **65 584 B/call**, one 65 536-byte
payload plus a small fixed residue. The **process-heap total is flat** between N = 200 and N = 800
(1 416 328 B at both), which is the counter-evidence against any collector-side retention: the
young/mature generations are not what grows. Peak RSS tracks the payload count at a slope of ≈65 KB
per call over a ≈113 MB boot baseline.

### The decisive negative result — an explicit collection reclaims nothing

The same probe with an explicit `erlang:garbage_collect()` every 100 calls changes **nothing**:
the roster len reads 2 → 102 → 202 → … → 801, `erlang:memory(binary)` is exactly **100 × 65 536 B**
at every probe, and the run ends at 52 483 632 B / roster 801 / peak RSS 168 024 KiB — against
52 483 632 B / 801 / 168 548 KiB without the extra collections.

A collection reclaims **no** off-heap payload whose owning handle lives outside the process heap: the
collector can only report which on-heap `BinRef`s are reachable, and the owner must act on that
report. That is what the two seams above are for, and it is also why the roster needs a **trigger**
that reaches it at all — a loop that reaches no heap-admission door runs no collection of either kind
to offer the reclamation, which is the role of the `BinVheap` trigger — in the runtime, at the
slice-boundary park (`beam_rt/src/context/gc/bin_vheap.rs`).

**Caveats.** The host is shared, so only **structural** figures are quoted as assertions: the
allocator's `live_bytes`/`carrier_bytes`, the roster length and its peak, the heap capacity and space
count, the collection counts, and the exact 65 536 B payload constant. **No wall clock appears in this
section.** The `VmHWM` figures are peak-RSS observations on that shared host rather than assertions,
and they carry a ≈113 MB boot baseline, so the load-bearing quantity is their **slope** against N,
never their absolute value; the in-crate ladder's 3 600 → 4 296 KiB span is four separate serialized
runs (one collector each, serialized because the allocator registry is process-global), not one
process growing. And the reader must not conclude from the runtime probe that this crate's collector
retains anything — the flat `erlang:memory(processes)` figure between N = 200 and N = 800 is the
counter-evidence. Every figure in this section is the pre-wiring one, and the table above is the
A/B's *before* side: the runtime drives both halves per process at its slice-boundary park, so its
`erlang:memory(binary)` is flat in the iteration count — the end-to-end figure, its
capacity-not-latency reading and the attribution live in `CONTEXT.md`'s `## Measured` bullet and are
measured by `beam_rt`'s `tests/off_heap_reclaim.rs`, not here. The in-crate ladder above remains this
crate's own counter contract.

## The accounting band (a footprint against the allocator's own live bytes)
`tests/heap_accounting.rs::accounted_bytes_track_the_allocators_live_bytes` builds a synthetic
workload — two `grow_nursery` calls over an 8 192-word nursery with 600 + 200 cons cells kept
live, plus 256 `RefcBinary` payloads of 4 096 B registered in a `beam_gc::OffHeap` — and compares
`HeapFootprint::heap_bytes()` against `beam_alloc::registry::all_stats()`'s summed `live_bytes`:

| quantity | value |
|---|---|
| `young_capacity_words` / `young_used_words` | 120 187 words (961 496 B) / 1 600 words (12 800 B) |
| `mature_capacity_words` / `mature_used_words` | 8 192 words / 0 |
| `off_heap_payload_bytes` | 1 048 576 B (256 × 4 096) |
| `off_heap_slot_bytes` | 16 384 B (256 reserved slots) |
| `heap_bytes()` | 2 010 072 B |
| `total_bytes()` | 2 026 456 B |
| allocator `live_bytes` (all types) | 2 022 432 B |
| **`heap_bytes() / live_bytes`** | **0.99389** |

The pinned band is **[0.90, 1.00]**: `heap_bytes() <= live_bytes` is the honesty direction (nothing
this crate accounts for may exceed what the allocators say they hold), and the 90 % floor is what
catches a silently dropped half. The 12 360 B remainder is entirely allocator-side and is named in
the test: 12 288 B = 256 × 48 B of `RefcBinary` control header plus size-class rounding (a 4 096 B
payload measures 4 144 live bytes), and 72 B = the four heap blocks' size-class rounding
(961 568 live for 961 496 reserved). Both halves are load-bearing: heap capacity is 47.5 % of
`live_bytes` and the payload is 51.9 %, so neither one alone can pass the 90 % floor.
**Stability:** five consecutive runs printed bit-identical figures (`heap_bytes=2010072,
total_bytes=2026456, live=2022432`); the workload tests serialise on a mutex because the allocator
registry is process-global. **Caveats.** Structural figures only — no wall clock, no RSS, and
`heap_bytes()` deliberately excludes the roster array (16 384 B here), which no allocator type
records; `total_bytes()` includes it. `heap_used_words()` counts the chain, so the retired space's
1 600 live words appear in the footprint where `young().used_words()` alone reports 0.

## What a reported `erlang:memory(total)` actually moved (m16 A/B, runtime-side)
Two release `beam_rt` binaries (m15-era `a34ca61332` vs that session's HEAD), one `-eval` probe
each, modal value of 20 / 12 runs: `total` 13 222 861 → 6 788 542 B, of which **`processes`**
(reserved process-heap capacity) accounts for −6 462 312 B, `code` +27 604 B, `atom` +917 B,
`emulator` −528 B, `binary` and `ets` unchanged. Both identities (`processes + system = total`,
`atom + binary + code + ets + emulator = system`) close exactly. The category map, the per-key
tables and the handoffs live in `src/ACCOUNTING.md`; this section records only the figures. Not a
crate assertion — these are the runtime's own totals, and the per-key spread across repeated runs
(`processes` in whole-heap-size steps of 1 872–6 024 B) is why each side is quoted as a modal value.
