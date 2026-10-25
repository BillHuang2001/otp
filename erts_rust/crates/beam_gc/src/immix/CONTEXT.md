# `beam_gc` — Mark-Region Space (`src/immix/`)

## Intent
`ImmixSpace`, the line/block mark-region mature generation the measurement-gated `MatureKind::Immix` uses: its
geometry, its line-granular allocation, its mark/classify/evacuate/finalize phases and the rollback primitives
its driver needs.
`../../CONTEXT.md` (crate) owns the seam shape and the strategy verdict, `../CONTEXT.md` (implementation node)
owns the sizing rules and the forensics, and `../immix_collector/CONTEXT.md` owns the collector that drives this
space; this file owns the space itself.

## Geometry and allocation
- Constants: `LINE_WORDS` 16, `LINES_PER_BLOCK` 16, `BLOCK_WORDS` 256.
- `ImmixSpace::blocks` is built once per **region** and mutated in place by every in-place reclaim; the space
  never reallocates.
- **Allocation is line-granular and needs a contiguous run inside ONE block**, so a fragmented region can refuse
  an object although region-wide `free_lines()` reports room — the refusal its driver must handle atomically
  (`../immix_collector/CONTEXT.md`).
- **`used_words()` is `used_lines() * LINE_WORDS`, an upper bound on the live words rather than an exact figure**,
  which is why no sizing rule reads it: a collapse is fed the words a promotion actually moved
  (`stats.words_copied`), exactly the quantity the copying collector feeds, so the two strategies' collapsed
  capacities are equal for the same live set.
- `IMMIX_BREAK_EVEN_WORDS = 8 * BLOCK_WORDS` (2048) is the automatic selection threshold
  (`../selection.rs`), resolved from the construction-time mature size or an explicit pin.

## Phases and rollback
- `collect.rs` holds the mark / classify / evacuate / finalize phases plus this space's `EvacDest` impl.
- **The `EvacDest` impl does not override `copied_top`, so it inherits the default `None`** — copies do not land
  in one contiguous monotone run, so the space is *unwalkable* for the linear relocator and keeps its own
  block-structured driver (`Relocator::new` refuses it with `BadArg`).
- **Invalidation is not reclamation.** `ImmixSpace::reset` only invalidates words inside a region that stays
  allocated; `release_dead_lines`/`finalize` clear bits inside a still-live region, and
  `release_words`/`release_lines` are the reservation-rollback primitives. None of these is an unmap-class free
  site — the region itself is released only when the whole `ImmixSpace` is replaced (see
  `../CONTEXT.md`, "Which of those sites unmaps and which merely recycles").

## Routing Table
- `mod.rs` → `ImmixSpace` geometry, block metadata, line-granular allocation, the `release_*` rollback
  primitives, `ImmixStats`/`ImmixCounters`
- `collect.rs` → the mark/classify/evacuate/finalize phases and the space's `EvacDest` impl
- `tests.rs` → the space's in-module suite (geometry, mark/pin, evacuation, accounting)
- `../immix_collector/` → the collector that drives this space (`GenImmixCollector`)
- `../selection.rs` → `IMMIX_BREAK_EVEN_WORDS`/`IMMIX_BREAK_EVEN_BLOCKS`, the automatic selection rule
