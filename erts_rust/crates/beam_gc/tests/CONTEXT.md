# `beam_gc` — Integration Suite (`tests/`)

## Intent
The out-of-crate evidence for `beam_gc`'s public API: every file here is a **separate crate**, so it
can only touch what the crate exports, and a change that keeps the in-module unit suite green while
narrowing (or silently re-shaping) what a *host* can do fails here.
Each file's `//!` module doc states the contract it pins and why — that is this suite's convention, and
the authority for what a file's cases prove. The one-line summaries in the routing table below are for
navigation only; do not restate a file's contract elsewhere.

Two conventions are load-bearing:
- **Fixtures may write raw words** (that is what a word region is), so `unsafe` is expected here — with a
  `# Safety` section on every `unsafe fn` and a `// SAFETY:` rationale before every `unsafe` block.
  Fixtures pre-fill every word with an immediate so a mistaken read can never dereference anything.
- **A submodule of a test file needs `#[path]`.** Each `tests/*.rs` IS a test-crate root, and rustc
  resolves a crate root's submodules in the root's own directory (`tests/`), not in `tests/<stem>/`, so a
  bare `mod foo;` would look for `tests/foo.rs` — which Cargo would then also build as a test target.

## Routing Table
- `mature_strategy_sweep.rs` → **the collector-level strategy sweep**: the barrier-free copying
  mature generation against the mark-region (`Immix`) one over the two workload classes a collector
  alone can express — C1 `churn` (low live set, high short-lived churn) and C2 `growth` (a live set
  that grows monotonically), plus the optional C3-shaped `copy_out` (`ets_copy`'s allocation geometry
  with no ETS and no runtime). One non-ignored `#[test]` is only a **driver**: it runs the arms
  strictly interleaved, one **fresh child process per (class, arm, round)** (this binary re-exec'd
  as the `#[ignore]`d `child_runs_one_arm_and_prints_a_row`, polled for `VmHWM` by the parent, with
  `/proc/loadavg` read before and after every round), prints one machine-readable `ROW …` line per
  arm plus a per-class estimator (the two arms' medians, the median paired difference
  `Immix − Copying`, the median paired ratio, and the per-round differences' `[min .. max]`), and a
  three-repeat **noise floor** of one arm. Both arms come only from `AnyCollector::from_config`
  with an explicit `GcConfig::copying`/`GcConfig::immix` pin over the **same** `HeapPolicy`
  (`GenImmixCollector::with_config` ignores `MatureKind`, so it is never used). Asserted: the arm's
  `kind()` and the config's `mature_choice()` (`MatureSource::Explicit`); `minors + majors ==
  collections == rounds == N` and the same from the collector's cumulative `GcStats`; `words_copied`
  / `objects_copied` / `mature_peak_words` all `> 0`; the footprint is an exact reserved-capacity
  account (`capacity_bytes == (young + mature) × 8` over an empty off-heap roster,
  `capacity >= used`); `mature_capacity >= live_words`; the live set is `intact` and its
  `Σ size_object` equals the class's geometry formula; the cadence floor `majors >= collections ÷
  (fullsweep_after + 1)` in both arms, exact (to within one fullsweep cycle) for the copying arm on
  the classes whose live set cannot outgrow a mature generation; both arms comparable; and the noise
  floor's deterministic counters **bit-identical** across its repeats. Wall time and peak RSS are
  printed, never bounded or compared; the only RSS assertion is the *definition* of the reported
  per-arm peak — the union `max(child's own VmHWM, parent's polled peak)`, since the child reads
  before it prints/exits while the poll runs until exit, so neither reading bounds the other.
  `N` is scaled on `cfg!(debug_assertions)`; `BEAM_GC_SWEEP_ROUNDS` /
  `BEAM_GC_SWEEP_N` override. The class geometry and its fixed-point reasoning live in
  `mature_strategy_sweep/fixtures.rs`.
- `mature_strategy_sweep/fixtures.rs` → its class-shape module (included by `#[path]`): the three
  geometries and their `HeapPolicy`/`GcConfig` builders, the per-round driver (`run` → `Tally`), and
  the independent live-set witness check. Nothing here is `#[test]`-annotated.
- `region_relocation.rs` → **acceptance evidence for region-generic relocating compaction**: that a host
  can evacuate a *foreign* word region (not a process heap) with `FromSet::from_ranges` +
  `Relocator`/`compact` into an `EvacDest`, and that the copy rules those drivers rest on are the real
  ones. 11 cases over a two-region grid (region A `ReservedHeap`-backed holding a tuple/fun/map, region B
  `Vec`-backed holding the leaves they reference, each region also holding one unreachable tuple):
  `cross_region_graph_relocates_with_its_structure_intact` (a — structure + rewritten roots + addresses
  changed), `unreachable_objects_are_not_copied` (b — exact `words_copied`/`words_reclaimed`, dead
  sentinel immediates absent from the destination, the dead source words never even forwarded),
  `per_slice_forwarding_accumulates_and_repeats_are_no_ops` (c — one `forward_roots` per root slice over
  two scattered Rust structs, counters are the union, a repeat is a no-op),
  `refc_binary_pair_copies_as_two_blocks_and_keeps_its_payload` (d — the `[BinRef][ErlSubBits]` pair is
  two allocations, `orig` forwarded, `BinRef.val`/refcount verbatim),
  `fun_descriptor_is_opaque_and_its_free_variables_relocate` (d — descriptor words copied verbatim, only
  env slots forwarded), `a_root_outside_the_ranges_is_left_byte_identical` (e — a foreign root and its
  subtree untouched, neither copied nor descended into),
  `walk_and_live_counters_agree_with_the_relocation` (f — `for_each_reachable_in`/`live_words_in` vs the
  copy's own counters), `an_undersized_destination_is_refused_atomically` (g — `AllocKind::Heap` refusal
  with source/roots/destination byte-identical, and `need - 1` proven to be the boundary),
  `dest_words_used_is_the_linear_copy_volume` (`dest_words_used == words_copied` for `LinearDest`, incl.
  an empty from-set), `an_out_of_crate_evac_dest_drives_the_same_relocation` (the host-implementable
  `EvacDest` seam: a `ReservedHeap`-backed destination defined in the test reproduces the reference run
  exactly), `an_unwalkable_destination_is_refused_before_mutating` (`copied_top() == None` ⇒ `BadArg`
  from all three entry points, before the first word moves).
- `region_relocation/fixtures.rs` → its fixture module (included by `#[path]`): `Region` (one foreign word
  area over a `Vec<Eterm>` **or** a `beam_alloc::ReservedHeap`, with the host's own `alloc` bump pointer,
  `snapshot`, and term builders `cons_list`/`tuple`/`flat_map`/`local_fun`), `from_set`, the graph readers
  (`header`/`tuple_elements`/`list_cells`/`copied_run`), and `build_grid` with the **pinned live geometry**
  (`LIVE_WORDS = 21`, `OBJECTS = 7`, `DEAD_WORDS = 6`) plus the dead-object sentinels. The pinned counts
  are what make a `move_size`/layout change surface as a count instead of a silent shift.
- The other files in this directory (copy/generation/chain/bounds/Immix/off-heap/accounting gates) are
  catalogued — with the contract each pins — in `../CONTEXT.md`'s "Test Strategy" and routing table.

## Constraints
- **Public API only.** No `crate::`/`pub(crate)` access, ever: these files are the dependent's view.
  `src/` is not modifiable from here — a genuine implementation bug is reported, not fixed.
- Fallible calls return `beam_common::Result`; match the typed error (`BeamError::Alloc` +
  `AllocKind::Heap`, `BeamError::BadArg`) instead of asserting "it failed". No `unwrap()` on runtime data;
  `.expect()` only on fixture construction the test fully controls.
- Apache-2.0 `// %CopyrightBegin% … // %CopyrightEnd%` block at the top of every `.rs` file, copied
  byte-exactly from a sibling (`scripts/license-header.es scan --path erts_rust/crates/beam_gc` must not
  name the file).
- Build/test via `erts_rust/scripts/cargo.sh` (sandbox `$HOME` is read-only; `--manifest-path
  erts_rust/Cargo.toml` is required from the repo root). Never `cd`.

## Notes for Agents
What a `beam_rt` driver must know about the relocation seam (each pinned by a case above):
- **A destination must publish a contiguous copied run** (`EvacDest::copied_top`). A line-granular or
  non-contiguous destination (the mark-region space's shape) returns the default `None`, which `Relocator`
  refuses with `BadArg` before any copy — such a host must bring its own driver, not `LinearDest`.
- **`compact` is atomic for one root slice; a `Relocator` is not.** `new`/`new_checked` mutate nothing and a
  too-small destination is caught by the preflight, so `compact`'s refusal leaves source, roots and
  destination byte-identical. Calling `forward_roots` directly and hitting `heap_exhausted` mid-copy leaves
  the source **partially forwarded** — unrecoverable; use `new_checked` with `live_words_in`'s figure.
- **`for_each_reachable_in` reports boxed objects only** (a cons cell has no header word: it is traversed,
  not reported), while `objects_copied`/`live_words_in` count cells too. The word totals reconcile
  (`boxed_words + 2 × cells`); a driver comparing the two accounts must expect the difference.
- **`move_size` is not `layout::object_words`** for a refc binary: the copy graph allocates the `ErlSubBits`
  as its own 5 words and forwards `orig` to a separate 3-word `BinRef` (8 words, 2 objects), even though
  `object_words` reports 8 for the pair as one object.
- `BinRef.val` (the off-heap payload pointer) and its refcount are **not** touched by a relocation: the
  payload never moves, so a host keeps its own liveness/refcount accounting.
- **Copying-vs-Immix cost A/Bs (same harness, same geometry, both strategies driven) live in**
  `allocation_heavy_workload.rs`, `immix_measure.rs`, `immix_workloads.rs`, `immix_break_even.rs`,
  `immix_selection_curve.rs`, `steady_state.rs`, `generational_cadence.rs`,
  `mature_capacity_convergence.rs`, `mature_capacity_feedback.rs`, `nursery_growth_retention.rs`,
  `offheap_steady_state.rs`, `retained_vs_live.rs` (the one that also samples RSS, `read_rss_bytes`),
  `post_fullsweep_nursery.rs`, `production_dispatch.rs` and `mature_strategy_sweep.rs` (the
  collector-level C1/C2/C3-shaped sweep, with per-class paired medians and a noise floor).
  `immix_parity.rs`, `gen_immix_collector.rs` and `immix_stress.rs` also drive both strategies but
  compare *observable results*, not cost.
- **No messaging/copy-bound and no real `compile:file/2` workload is measured under both strategies
  here** — that remains true: this crate's dev-deps are `beam_codec` only, so a `beam_message`/`beam_rt`
  shape cannot be driven from this suite. `mature_strategy_sweep.rs` is the **in-crate counterpart**
  (the collector-level C1/C2 sweep plus a C3-*shaped* copy-out with no ETS); the real C3
  (messaging/copy-bound) and C4 (a real in-process compile) measurements live in
  `../../beam_rt/GC_STRATEGY_MEASURED.md` and their harness in `../../beam_rt/tests/gc_strategy_classes.rs`.
- **No per-file wall time is recorded anywhere**; the only in-crate figure is
  `nursery_growth_retention.rs`'s whole-5-test file at 0.01 s (`../MEASURED.md:497`). Nothing under
  `tests/` is `#[ignore]`d (the crate's only two ignores are doctest fences, `src/offheap.rs:102,153`).
- `AnyCollector`'s variants are **public** (`Copying(Collector)` / `Immix(GenImmixCollector)`,
  `../src/any_collector.rs:52`), so a test reaches `ImmixStats`
  (`GenImmixCollector::last_immix_stats`, `../src/immix_collector.rs:280`) and `mature().used_lines()`
  with a two-arm match; `AnyCollector` itself exposes only `GcStats`, `HeapObservable` and
  `HeapFootprint`.

## Test Strategy
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc`
  (whole crate, incl. this directory) and `… -p beam_gc --test region_relocation` for the focused run.
- Cross-checks against the real system ERTS belong to `../tools/differential/`, not here.
