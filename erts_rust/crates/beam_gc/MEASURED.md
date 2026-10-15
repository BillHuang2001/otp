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
* [End-to-end RSS and latency](#end-to-end-rss-and-latency-ets_copy-a-b-in-this-worktree) — the
  nursery-chain fix measured on a real `ets_copy` loop (pre-fix vs post-fix).

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

## Nursery growth retention: the chain is bounded (copy work, retained capacity, latency)

**The question.** `Nursery::grow` is the *no-relocation* growth seam: it never collects and never moves a
term, so the space it supersedes must stay addressable until a collection evacuates it. What bounds the
chain — and what happens at a loop workload's heap-admission point when the grown space has room?

**The corpus.** A grow + heap-admission cycle repeated 400 times: the engine's `test_heap` prefix asks
`should_collect(16)` and collects over its published roots when the answer is true; the op's own un-rooted
result is a 256-word tuple (exactly one whole mark-region block), allocated through the grow-only seam
when it does not fit. Driven through **both** mature strategies. Deterministic; timings printed only.

| | chain length (peak, spaces) | retained capacity (peak, words) | peak current nursery (words) |
|---|---|---|---|
| **Pre-fix** (`HEAD~1`) | **401** | 401 whole spaces | 752 |
| **Post-fix** | **2** | 752 (= the largest single space installed) | 752 |

Pre-fix the chain lengthens **once per cycle** — the retention scales with the iteration count, exactly
the per-op heap retention a loop workload's RSS followed. Post-fix it is **one retired space at most**,
however many cycles run, and the run converges to a **period-2 plateau** (4 distinct tail states, a fixed
mature capacity, a fixed pair of nursery capacities). Both strategies reach the same plateau, which is why
the fix's second half — the *collection-debt* half of `should_collect` — is what makes the retention
bounded: with only the occupancy half (the pre-fix arithmetic) the chain grows forever, because `collapse`
runs *only* inside a collection and no collection was ever asked for.

**Cost.** The fix adds **no per-allocation work**: the nursery bump path is untouched (the growth check
runs only where the allocator already branched to grow), and the only added work is **one collection per
growth episode** at the *next* heap-admission point — the correct generational cadence, not a new tax. The
pin test's whole loop, including those collections, runs in **<10 ms** (400 cycles) on the debug-level
build; the crate's deterministic counts above are the load-invariant evidence and are asserted in
`tests/nursery_growth_retention.rs`. An external `tools/bench` RSS re-run (`ets_copy`,
`gc_bound/churn`) was **not** completed within this pass — see Caveats.

**Superseded-space rule, measured directly.** A growth that supersedes a space which never handed the
mutator a word is dropped rather than retired: `count` unchanged (1), retained capacity 0, and no debt
reported. The growth that follows a collection (`ensure_capacity`'s collect-then-grow pairing) supersedes
exactly such an empty space, which is what breaks the collect/grow oscillation — measured: a
collect→grow→admit sequence leaves `young_space_count() == 1` and `should_collect(0) == false`.

**Pre-existing, NOT caused by this fix: mark-region contiguity.** Driving the same cycle with a
**100-word** (7-line) tuple at a 2048-word region fails after **43 rounds** with
`Alloc(AllocFailure { requested: 800, kind: Heap })` while `mature_capacity_words()` still reports 2048.
Cause: `ImmixSpace::alloc_words` needs a run of the object's 7 lines **inside one 16-line block**
(`find_run`/`first_hole`), while the promote/reclaim pre-check compares against the region-wide
`free_lines()`; two 7-line objects fill 14 of a block's 16 lines, so the region reports 8 free lines no
single run can use and the promotion fails. **Proven independent of this change**: the identical corpus
reproduces a **byte-identical** failure (same round 43, `mature=1024` variant at round 18) with the
retention fix reverted to `HEAD~1`, and with an occupancy-only admission signal (chain length 1
throughout — the retention path not exercised at all). Recorded in `CONTEXT.md` "Known Gaps"; the pin test
deliberately uses a block-aligned 256-word object so it pins the retention, not this gap.

**Caveats.**
* **The external RSS/latency figures are in [the end-to-end section below](#end-to-end-rss-and-latency-ets_copy-a-b-in-this-worktree).**
  They show the mechanism fix is **necessary but not sufficient**: on a real `ets_copy` loop the process
  RSS is **unchanged** by this fix, because the runtime's heap-admission *door* declines the collection
  that would `collapse` the chain. The in-process pre/post chain measurement above is the load-invariant
  evidence for the mechanism itself (retained spaces); the two are not interchangeable.
* **Retained *words* are a capacity figure, not RSS.** A retired `HeapSpace` holds a whole backing
  allocation, so "2 spaces instead of 401" is the RSS-relevant quantity, but the two are not numerically
  interchangeable (allocator carriering, page granularity, and the rest of the process's footprint sit on
  top).
* **A pure un-rooted growth loop still grows the chain** — it never reaches a heap-admission point, so
  nothing claims the debt. That is pinned deliberately by `beam_rt`'s own
  `alloc_words_grows_the_nursery_and_never_falls_back_to_the_arena` (it asserts `young_space_count() > 1`).
  Every real Erlang loop reaches an admission point at its `test_heap` prefixes, where the retention is
  bounded from that point on.

## End-to-end RSS and latency (`ets_copy`, A/B in this worktree)
**What was run.** The real runtime binary (`erts_rust/target/release/beam_rt`, release, one scheduler) on
`tools/bench`'s own `copy_bound` loop body, reconstructed and cross-checked against the bench tool:
`beam_rt -pa bin run bin/bench_copy_bound.beam bench_copy_bound:ets_copy/1 <N>` (the `/1` arity suffix is
required). Peak-granularity `VmHWM` plus a ~4 ms `VmRSS` series sampled from `/proc/<pid>/status` by hand
(`/usr/bin/time` is absent); wall-clock around the child. A/B: the same binary built twice — **AFTER** =
`beam_gc` at `5109182990`, **BEFORE** = `beam_gc` at `0f5e2130ca` (`git checkout <sha> -- .../beam_gc/src
.../beam_gc/tests`, build, restore; no `git stash`).
| N | VmHWM kB AFTER (2 reps) | VmHWM kB BEFORE (2 reps) | wall ms AFTER | wall ms BEFORE |
|---|---|---|---|---|
| 0 | 385604 / 385644 | 385512 / 385576 | 2280 / 2349 / 2590 | 2111 / 2134 / 2153 |
| 1 000 | 386352 / 386352 | 386376 / 386292 | 2237 / 2220 | 2222 / 2083 |
| 10 000 | 393200 / 393212 | 393076 / 393216 | 2265 / 2258 | 2488 / 2114 |
| 25 000 | 404044 / 404148 | 404092 / 403876 | 2430 / 2421 / 2468 | 2215 / 2245 / 2155 |
| 50 000 | 422864 / 423132 | 422776 / 422964 | 2533 / 2476 | 2264 / 2247 |
| 100 000 | **461012 / 460856** | **460868 / 460628** | 3747 / 4015 / 3580 / 3874 | 2788 / 2548 / 2476 / 2582 |
**Verdict: the RSS is NOT bounded by this fix, and the before/after curves are identical** (within
run-to-run noise) — ≈ **376.5 MiB fixed baseline** at N=0, **+≈750 B (~94 words) per iteration**, ≈
**450 MiB** at N=100 000. The bench tool's own figure matches: `case_ets_copy_peak_rss_bytes` mean
**450.1 MiB** (471 926 101 B), `ns_per_op` 4717.8, `wall_ms` 2599, `startup_ms` 2127; the same tool's
`gc_bound/churn` control reads **376.6 MiB** — i.e. the *baseline only*, ~0 marginal growth where the loop
is GC-bounded. So this workload is **not** GC-bounded, and the fix (whose in-crate effect is proven above)
never gets to act: `process_info(self(), memory)` grows 3 728 B → **75 203 768 B** at N=100 000 with
**`minor_gcs = 0`** throughout, while the same loop with the container result not copied out
(`ets_copy_nogc`, and a named-table variant alike) stays at **5 624 B**. The blocker is therefore
downstream of `beam_gc`: `beam_rt`'s heap-admission door declines the collection
(`src/context/gc.rs`'s unscanned-root valve `any_root_is_unscanned` / the pointer-stable arena escape of
`move_result_into_process_heap`; the caller-module float guard `module_may_be_collected` is **not** the
mover — the module has no float ops). Not chased further; it is outside this crate.
**Cost: material at high N.** Interleaved reps on a heavily loaded host (`/proc/loadavg` 84–128, `nproc`
14; the RSS sweep itself ran at load 10–34) show AFTER consistently dearer: **+1 205 ms at N=100 000**
(4/4 reps), ≈ +235 ms at N=25 000, and within the ±250 ms noise floor at N=0 (no loop work). That is a
marginal ≈ **5 µs/op before vs ≈ 18 µs/op after** (slope between N=25 000 and N=100 000) — consistent
with the now-attempted collections whose *checks* run and then decline (no `minor_gcs` increments), i.e.
the same blocker, not a new per-allocation tax on the bump path.
**Caveats.** Reconstructed harness (the sibling `tools/bench` N is a compile-time constant and is
read-only here); `VmRSS_first` is unusable (pure sample-timing noise, 2.4–9.1 MB while `VmHWM` was already
385 MB); the ~376.5 MiB boot/`VmSize` baseline (peak `VmSize` 1 000 812 kB) dominates at these N; no
`/usr/bin/time`; raw series/artifacts were kept outside the repository.
