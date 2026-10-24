# `beam_rt` — what a `beam_rt` startup costs, and where the cost is

The startup record: the retired-instruction floor of one `beam_rt -noshell -eval 'halt().'`
invocation, the paired delta each landed change bought, the current profile's self-time split, the
boot's collection accounting with its mechanism, and the two cross-node changes the remaining cost
needs (written as patches for their owning crates).
The instrument is a **retired-instruction count**, never a wall time: the figures below are
`perf record -e instructions:u -c 2000000 --call-graph dwarf,16384` over
`./target/release/beam_rt -noshell -eval 'halt().'`, pinned `taskset -c 2`, with the two binaries
alternated inside every round and each round's `/proc/loadavg` recorded.
**Every number here is one host's** (a hybrid-CPU machine whose P-cores are 0–3, shared with other
work): the deterministic counter is the signal, and a wall figure is quoted only where the effect
is far above the host's own spread.

## The floor

| point | retired instructions | wall p50 | ratio vs. the unmodified control |
|---|---|---|---|
| campaign start (the control) | 2 045 799 320 | ≈ 220 ms | 1.00 |
| current | 1 776 M | ≈ 192.5 ms | 0.87 |

The control is the base commit **rebuilt from its own source**, never a stashed binary: a
from-source rebuild of identical source is instruction-identical but 2–4 % apart in wall, so a
kept binary cannot serve as a control.
`−13.2 %` of the boot is what the changes below account for: their paired deltas sum to 13.5 % of the
control, so they add up to the floor within the per-round spread, and the residual is dispatch, atom
intern, the codec and the allocator (the profile below names each share).

## The landed changes and their paired deltas

| change | paired delta | instruction figures |
|---|---|---|
| the GC valve walk (four landings) | **−3.166 %** | 2 045.8 M → 1 981.0 M |
| `CodeServer::add_system_paths` installed once per code server | **−2.40 %** | 47 588 853 instructions |
| the process-exit teardown skipped on the binary's path | **−7.94 %** | ≈ 157 M / ≈ 25 ms |
| `init:script_id/0` derived from the release | correctness, not cost | — |

**The GC valve walk's four landings** are, in order: the mature-first ordered scanned test with the
single-nursery-space shortcut (`a36873b8f0`), the off-by-default invocation census plus the per-pid
admissions/collections block (`f9e22744af`), the mutator block hoisted out into the walk's geometry
(`caab3d3f90`), and the visited set's room-making moved out of the insert hot path
(`1304a6b44c`) — which is also why `Visited::insert` and `Walk::seed` still appear in the profile
below: the walk is cheaper per call, not absent.
**The one-shot system-path install** replaced a per-call resolution: `code:get_path/0` cost 1.685 M
instructions per probe before it and 0.529 M after it, and the boot no longer re-derives the 36-entry
path on every `code:*` read.
**The exit-teardown skip** is `crate::cli::run_for_process` → `ExitTeardown::Leak`: the binary does
not drop the boot's retained state (every `Arc<Module>`, the JIT artifact cache, `LabelMap`s, the
≈ 3 000 `Arc<str>` atom names) at `halt`.
It buys ≈ 25 ms per invocation and it moves **no** peak `VmHWM`, at the cost of the process's
last-instant RSS — the trade, its measured probe figures and the deliberate `malloc_trim` placement
behind it are [`src/malloc.rs`](src/malloc.rs)'s module docs and `CONTEXT.md`'s `### malloc.rs`
section.
**`init:script_id/0`** is not a performance change: it derives the script id from the release the
runtime actually booted instead of a literal, and it is recorded here because it was measured in the
same pass and must not be read as a delta.

## The current profile — self-time

`perf report --no-children` over the same invocation:

| symbol | self-time |
|---|---|
| `__libc_malloc2` | 18.57 % |
| `Visited::insert` | 9.54 % |
| `_int_free_chunk` | 6.67 % |
| `Walk::seed` | 4.98 % |
| `<HashMap<Arc<str>, Atom>>::insert` | 4.39 % |
| `group_functions` | 3.71 % |
| `md5_compress` | 3.38 % |

The shape is a **module-graph load**: an allocator share that is mostly the atom table's name
strings and the decoded instruction vectors, a GC valve walk over the loaded graph, and the MD5 that
seals each module.

## `CodeServer::load_bytes` — the inclusive split (47.02 % of the run)

| phase | inclusive |
|---|---|
| `lower::build_module` | 17.49 % |
| `<AtomTable as Clone>::clone` at `src/codeserver.rs:919` | 15.07 % (298 M) |
| `decode::decode_stream` | 9.00 % |
| dropping the decoded `Vec<RawInstr>` | 4.04 % |

The `AtomTable::clone` is the single largest removable item in the run: `build_module` takes a
*cloned* atom table so it can append a literal-only atom without touching the shared one, and
`Clone` is deliberately a **deep** copy (see the ask below), so every module load materializes one
`Arc<str>` per atom in the table.
The first ask replaces exactly that call.

## Boot collection accounting — 427 collections per boot

One bare boot runs **427 collections**: 165 are **occupancy-driven** (a heap admission found the
nursery full) and 262 are **`collection_debt`-driven** (a grown, never-collected space was retired
by a growth), with a mean of **1 322 free words** at admission, **237 of 427** leaving the capacity
unchanged, and pid `<0.10.0>` owning **398** of them.

### The mechanism is a shrink/grow oscillation, not the live set

`src/context/calls.rs::hint_grow_gate` → `Process::grow_nursery` retires the space it replaces
(that is `Process::collection_debt`), and the settling collection's `Nursery::collapse` calls
`beam_gc::HeapPolicy::young_after_collection`, which shrinks the nursery to
`young_minor_target_words(≈ 220)` = **233 words** because this rule shrinks whenever
`capacity > 8 × target`.
The C gates that branch on the heap it is about to replace: `HEAP_SIZE(p) > 3000`
(`../../erts/emulator/beam/erl_gc.c:1517-1529`).
The observed ladder on one boot is `2 586 → 376 → 986 → 233 → 609 → 1 596 → 376 → 1 974`: every
collapse re-installs the floor, the next allocation growth retires it, and the debt it leaves forces
the next collection — so the 427 are a **capacity oscillation**, not the live set asking to be
collected.

### The `+hms` sweep — a floor by proxy (5 paired rounds per row)

| `+hms` | collections | retired instructions | VmHWM |
|---|---|---|---|
| none | 427 | 1 980.5 M | 116.3 MB |
| 1000 | 91 | 1 714.8 M (−13.48 %) | — |
| 3000 | 41 | 1 674.8 M (−15.44 %), cycles −8.8 % | — |
| 8000 | 13 | 1 644.6 M (−16.96 %) | — |

The sweep ran against the then-current binary, so its `none` row is that round's own control (not
the current floor above): the percentages are within-sweep paired figures.
A heap floor is therefore worth **−13.5 % to −17 %** and, measured on the `8000` row, it **also**
lowers peak `VmHWM`: less copying *and* less memory, which is why the second ask is a floor rather
than a tuning knob.

## Cross-node asks — the two changes this crate cannot make

Both are owned by a sibling crate and are written here as a patch to apply, with the measured value
and the tests that must accompany it.
Nothing in `beam_rt` needs to change for either.

### (a) `beam_term`: `AtomTable::fork(&self) -> AtomTable`

`Clone` must keep its deliberate deep-copy meaning — its rustdoc (`../beam_term/src/atom.rs:120-140`)
records that `beam_rt/tests/atom_table_commit_alloc.rs` and `beam_bifs/tests/decode_hot_path.rs` use
`clone()` as a **positive control that must scale with the table**, so sharing the names inside
`clone` would silently invalidate two suites' premise.
Add a second constructor instead, sharing the `Arc<str>` handles with a fresh `instance` and the
same `generation`, preserving every index:

```rust
impl AtomTable {
    /// Shares the name handles with a **fresh** identity and the **same** generation, so every
    /// index resolves exactly as it does in `self` — the read-only fork a consumer wants when it
    /// may *append* (a literal-only atom) but must not deep-copy Θ(N) strings to do it.
    ///
    /// Unlike [`Clone`] this allocates no name string: `names` clones as `Arc<str>` handles and
    /// `index`'s keys clone as handles too. The fresh `instance` is what keeps
    /// [`AtomTable::appended_since`] false in **both** directions (a fork diverges from its parent
    /// by independent appends, so neither snapshot may validate against the other), and inheriting
    /// `generation` preserves the prefix's meaning.
    pub fn fork(&self) -> AtomTable {
        debug_assert_eq!(
            self.index.len(),
            self.names.len(),
            "the index holds exactly one entry per interned name"
        );
        AtomTable {
            names: self.names.clone(),
            index: self.index.clone(),
            instance: next_instance(),
            generation: self.generation,
        }
    }
}
```

The callsite becomes one word (`src/codeserver.rs:919`):

```diff
-        let module = lower::build_module(&chunks, &raw, (*self.atoms).clone())?;
+        let module = lower::build_module(&chunks, &raw, (*self.atoms).fork())?;
```

**Value: −298 M instructions / −15.07 % of the boot.**
The tests it needs:
* a hermetic case asserting every shared index resolves to the same name in the fork and the parent;
* `Arc::ptr_eq` on the shared names (the fork shares rather than copies);
* appending to the fork leaves the parent's `len` and `generation` unchanged;
* `appended_since` is **false** in both directions.

### (b) `beam_gc`: a minor-shrink floor

`src/policy.rs::young_after_collection`'s shrink condition gains the C's floor:

```diff
+/// Never shrink a nursery below this capacity, however small the live set it just promoted.
+///
+/// Mirrors the C's `HEAP_SIZE(p) > 3000` gate on the same branch
+/// (`../../erts/emulator/beam/erl_gc.c:1517-1529`): without it a settling collection collapses
+/// the nursery to the floor, the next growth retires that space as collection debt, and the boot
+/// oscillates — 427 collections for one boot, whose admissions averaged 1 322 free words.
+pub const MINOR_SHRINK_FLOOR_WORDS: usize = 3000;
+
     pub fn young_after_collection(&self, capacity_words: usize, live_words: usize) -> usize {
         let target = self.young_minor_target_words(live_words);
-        if capacity_words > YOUNG_SHRINK_FACTOR.saturating_mul(target) || capacity_words < target {
+        let far_above_the_live_set =
+            capacity_words > YOUNG_SHRINK_FACTOR.saturating_mul(target)
+                && capacity_words > MINOR_SHRINK_FLOOR_WORDS;
+        if far_above_the_live_set || capacity_words < target {
             target
         } else {
             capacity_words
         }
     }
```

The rustdoc at `src/policy.rs:583-586` (the paragraph explaining that the band's upper edge stays
`YOUNG_SHRINK_FACTOR` wide) is amended to say the shrink branch additionally requires the capacity
to exceed [`MINOR_SHRINK_FLOOR_WORDS`], and to name the C gate it comes from.
**Value: boot collections 427 → ≈ 41, i.e. −13.5 % … −15.4 % of the boot** (the `+hms` sweep's
proxy above), with peak `VmHWM` slightly lower rather than higher.
**Blast radius.** The changed band is `(1 864, 3 000]` — the capacities that `8 × 233` no longer
covers but the floor does.
Nothing at or below 1 864 moves: the 233-word nursery floor, 843 (`heap_growth`) and 987
(`reds_boundary`) all stand, as do `bif_extra/process_info_spawned_exact_item_values`'
`{heap_size,233}`/`{total_heap_size,233}`.
Two assertions do need re-derivation: `src/runner/tests/boundary_cost.rs`'s
`MEASURED_YOUNG_CAPACITY = 2_585` (a capacity inside the new never-shrink band) and
`src/context/tests.rs::the_copy_bound_ping_pong_…` (whose settle point is the same rule).
The inherited red tests listed in [`TEST_STRATEGY.md`](TEST_STRATEGY.md) *Known-red tests at this
commit* are collection-cadence/capacity assertions of exactly this family, and this ask is their
most likely single cause.

## Measurement discipline

* **Pin, and read the P-core's counter.** The host is hybrid (`cpu_core` 0–3), so an unpinned
  `instructions:u` drifts by an order of magnitude; always `taskset -c 2`, and read
  `cpu_core/instructions:u` and `cpu_core/cycles:u` only.
* **A stashed binary is never a control.** Rebuild the control from its own source at the base
  commit, and alternate the two binaries **inside** every round.
* **Quote retired instructions for anything under ~3 %.** The counter is deterministic to ≈ ±0.1 %;
  a wall figure at that size is host noise.
* **Record the load.** `/proc/loadavg` before and after every round — this host is shared, so a
  comparison without it is not a comparison.
* **The record instrument is [`../../tools/bench`](../../tools/bench)**:
  `run --runtime all --quick --workloads baseline`, with `$BEAM_RT` pointing at an alternate binary.
  It is the sibling tooling node and read-only to this crate.

* **Measure this crate's per-call paths in retired instructions, not wall clock.** Pin to a P-core (`taskset -c 2`; the host is hybrid, so an unpinned `instructions:u` drifts ~10×) and read `cpu_core/instructions`. A from-source rebuild of the *same* source is bit-identical in instructions but differs by 2–4 % in **wall**, so a stashed binary is never a control and a two-build wall spread is the floor; `foreign`'s `ns_per_call` on the two short cases sits below it. Alternate the two binaries **inside** each measurement round.


## See Also

* [`PERF.md`](PERF.md) *Current startup regime* → the whole-process wall/CPU characterisation and the
  boot path's poll-budget note; the tier effect and the one-shot cold-start characterisation.
* [`PERF_HISTORY.md`](PERF_HISTORY.md) → the dated `BEFORE`/`AFTER` snapshots and the
  single-mechanism controlled A/Bs.
* [`HANDOFF.md`](HANDOFF.md) → the outstanding cross-node asks, of which the two above are the
  startup pass's.
* [`M1_GAPS_GC.md`](M1_GAPS_GC.md) *GC / heap model* and *Heap switches* → the collection points,
  the valves and the heap policy this file's second ask prices.
* [`src/context/gc/CONTEXT.md`](src/context/gc/CONTEXT.md) → the valve walk the first landed change
  made cheaper, and its per-pid census.
* [`src/codeserver/CONTEXT.md`](src/codeserver/CONTEXT.md) → the code store whose load dominates the
  profile.
