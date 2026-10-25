# `beam_gc` — Mark-Region Collector (`src/immix_collector/`)

## Intent
`GenImmixCollector`: the **same copying nursery** as the default `Collector` paired with a mark-region
`ImmixSpace` mature generation — its atomic two-phase promotion, its reclaim-first major, and the fact that the
strategy switch itself lives one crate-level above it (`AnyCollector`).
`../../CONTEXT.md` (crate) owns the seam shape and the measured strategy verdict; `../CONTEXT.md`
(implementation node) owns the shared sizing rules; `../immix/CONTEXT.md` owns the space. Files: this module,
`promote.rs` (the promotion driver), `tests.rs`/`grow_tests.rs` (the in-module suites).

## Design Decisions
- **A minor promotes through an ATOMIC two-phase driver** (`promote.rs`, `Result<PromoteOutcome>`): phase R
  reserves each first-time live object, copies its words and writes the forwarding marker but rewrites **no**
  reference word; phase C rewrites the recorded slots and roots once every object of the traversal is placed.
  The barrier-free invariant holds because promotion only ever moves young objects *into* mature, and a
  reservation that finds no run rolls both spaces back byte-for-byte (`PromoteOutcome::WouldNotFit`) instead of
  leaving a half-forwarded heap.
- **A refused promotion is atomic; `collect` recovers, `collect_minor` does not.**
  Line-granular allocation needs a contiguous run inside one 16-line block (`../immix/CONTEXT.md`), so a
  fragmented region can refuse an object although region-wide `free_lines()` reports room; the driver then rolls
  BOTH spaces back byte-for-byte — no marker, no rewritten root, no counter advanced.
  `collect` escalates to a fullsweep and `grow_major` rebuilds a fresh region, even at the current capacity when a
  ceiling forbids growing; only a ceiling that cannot hold the live set in ANY layout reports the typed
  exhaustion, and `collect_minor`'s own contract reports that error rather than escalating, which is `collect`'s
  job.
- **The major is reclaim-first, and sound for the young→mature edge an in-place reclaim cannot ignore.**
  A read-only `scan_young` reports the promotion's exact **line** count and every nursery word pointing into
  mature; `ImmixSpace::collect` takes those terms as its `promoted` roots (a mature object reachable only through
  the live nursery survives), each recorded nursery slot is rewritten to the object's final address, and only
  then does promotion run — by which point the nursery's mature references are `Foreign` and final.
  Reclaim-first is what lets a would-not-fit minor escalate to a major that *makes room*; `words_reclaimed` =
  mature reclaim + nursery garbage.
- **The region never reallocates in place; it is grown by relocation.** A major replaces the whole region — the
  live set relocated into a fresh `ImmixSpace` sized by the policy (`relocate_major`), or a larger one when an
  in-place fullsweep's reclaim cannot fit the nursery's promotion (`grow_major`) — so a growable heap never fails
  a collectable collection; only a `max_heap_words` ceiling the promotion cannot fit returns the typed
  `heap_exhausted` (before any copy).
- **`GenImmixCollector` ignores the `MatureKind` in `with_config`, and `AnyCollector` only forwards.** The type
  *is* the Immix-backed collector, so `with_config` always builds one over `cfg.policy()`; the strategy is decided
  by `AnyCollector::from_config` (on `config.mature()`) / `GcConfig::mature_choice`, and the dispatch seam carries
  no collection logic, so neither strategy depends on the other; the geometry accessors on `AnyCollector` exist
  only because the two mature spaces are differently typed.
- **The strategy is reachable in production; the DEFAULT remains the copying `Collector`.**
  `select_mature` chooses `Immix` only at/above `IMMIX_BREAK_EVEN_WORDS` (2048), which the default 233-word policy
  never reaches, so `GcConfig::default()` builds the copying `Collector`; Immix needs a large configured mature
  generation (`beam_rt`'s default-OFF `BEAM_RT_GC_STRATEGY=immix` opt-in) or an explicit pin — and the mature
  generation is genuinely exercised either way (**203 minors : 17 fullsweeps** on the copying default,
  `../../MEASURED.md`'s cadence table).
  The switch itself is adopted (`beam_process::Process` owns an `AnyCollector` built from the spawn policy,
  `beam_rt` drives it at its heap-admission point); the residual is that an explicit `GcConfig`/`MatureSelection`
  pin would need a parameter on the process-construction path (`ProcessTable::spawn` takes only a `HeapPolicy`) —
  owner `beam_process`.

## Per-call allocations (this strategy, no scratch reuse)
- `scan_young` allocates a `HashSet<usize>`, a work `Vec` and a `refs: Vec<(*mut Eterm, Eterm)>` — once per
  `collect`, `collect_minor` and `collect_major`; `promote`'s `PromotePlan::default()` adds three more `Vec`s
  (`work`/`rewrites`/`copies`).
- A major adds `live_full`'s own from-set + walk, `promoted: Vec<Eterm> = refs.iter().map(..).collect()` and,
  inside `ImmixSpace::collect`, `mark`'s `work: Vec<Eterm>`, `occupied_before: Vec<bool>::with_capacity(blocks)`
  and `evacuate`'s `work: Vec<*mut Eterm>`; a relocation/growth installs a whole fresh `ImmixSpace`.
- No field of the collector is a scratch buffer (the spaces, the counters and `last_immix`), so every figure
  above is a fresh allocation per collection call.

## Strategy parity and divergence
- **Both strategies run the same collection *kind* on the same cadence once both spaces are fixed points of the
  sizing rules** (`../tests/production_dispatch.rs`): the nursery half is shared, so under a policy whose nursery
  is its own live-set target and whose mature region keeps every fullsweep's `need_after` inside its keep band,
  the copying and the mark-region run ask the same admission questions, run the same kinds, and answer the same
  `heap_info` field for field.
  The mature generations are otherwise differently sized (**987** vs **1 024** words at the default policy,
  `../tests/steady_state.rs`), and a policy that lets either space resize can diverge in kind, because the
  mark-region space's line-granular accounting makes the two runs promote different word counts.

## Routing Table
- `promote.rs` → the ATOMIC two-phase promotion driver (`promote`, `PromoteOutcome`, `PromotePlan`, `rollback`)
- `tests.rs` → the collector's in-module suite: promotion, the atomic rollback (mutation-verified),
  reclaim-first ordering, kind parity
- `grow_tests.rs` → the `grow_major`/`relocate_major` region-growth tests
- `../immix_collector.rs` → the implementation (`GenImmixCollector`: `scan_young`, fullsweep sizing via
  `live_full`, `grow_major`/`relocate_major`, `ensure_capacity`, `last_immix_stats`, and the
  collect/collect_minor/collect_major/collect_kind driver)
- `../immix/` → the space this collector drives (geometry, line-granular allocation, phases)
- `../any_collector.rs` → the `Copying`/`Immix` dispatch and the strategy decision
