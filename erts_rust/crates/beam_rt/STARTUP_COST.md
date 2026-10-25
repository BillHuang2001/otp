# `beam_rt` — what a `beam_rt` startup costs, and where the cost is

The startup record: the retired-instruction floor of one `beam_rt -noshell -eval 'halt().'`
invocation, the paired delta each landed change bought, the current profile's self-time split, the
boot's collection accounting with its mechanism, and the two cross-node asks — both answered, with
what each answer is and what it is worth.
The instrument is a **retired-instruction count**, never a wall time: the figures below are
`perf stat -e cpu_core/instructions/u` / `perf record -e cpu_core/instructions/u -c 2000000
--call-graph dwarf,16384` over `./target/release/beam_rt -noshell -eval 'halt().'`, pinned
`taskset -c 2` on a P-core, with the two binaries alternated inside every round and each round's
`/proc/loadavg` recorded.
**Every number here is one host's** (a hybrid-CPU machine whose P-cores are 0–3, shared with other
work): the deterministic counter is the signal, and a wall figure is quoted only where the effect
is far above the host's own spread.

## The floor

| point | retired instructions | wall p50 | ratio vs. the unmodified control |
|---|---|---|---|
| campaign start (the control) | 2 045 799 320 | ≈ 220 ms | 1.00 |
| the atom-table fork's own control | 1 749.4 M | — | 0.855 |
| current | 1 473.3 M | — | 0.720 |

The control is the base commit **rebuilt from its own source**, never a stashed binary: a
from-source rebuild of identical source is instruction-identical but 2–4 % apart in wall, so a
kept binary cannot serve as a control.
`−28.0 %` of the boot is what the changes below account for, and each of their deltas is a **paired**
measurement against its own from-source rebuild — which is why the fork's control (1 749.4 M) is a
row of its own and is not the campaign-start figure: the two are different rounds of the same tree
lineage.
The current rows quote no wall figure: the host's own spread on wall is 2–4 %, so retired
instructions are the signal and a wall figure is only quoted where the effect is far above that
spread (the campaign-start `≈ 220 ms` is the one such figure).
What the floor still holds and no single change owns is the profile below: dispatch, atom intern,
the codec and the allocator.

## The landed changes and their paired deltas

| change | paired delta | instruction figures |
|---|---|---|
| the per-module atom-table **fork** adopted at the `CodeServer` callsite | **−15.78 %** | 1 749.4 M → 1 473.3 M (−276.1 M) |
| the GC valve walk (four landings) | **−3.166 %** | 2 045.8 M → 1 981.0 M |
| `CodeServer::add_system_paths` installed once per code server | **−2.40 %** | 47 588 853 instructions |
| the process-exit teardown skipped on the binary's path | **−7.94 %** | ≈ 157 M / ≈ 25 ms |
| `init:script_id/0` derived from the release | correctness, not cost | — |

**The atom-table fork** is the largest single item the profile ever carried: `CodeServer::load_bytes`
(`src/codeserver.rs:921`) hands `lower::build_module` `self.atoms.fork()` instead of a deep `Clone`,
and `src/eval/mod.rs`'s `cook_supplied_trace` does the same, so a module load shares the `Arc<str>`
name handles and preserves every index and the generation for a fresh `instance`.
The paired measurement is the median of **6 alternating paired rounds**, `cpu_core/instructions/u`
under `taskset -c 2`, noise band ±0.08 %; §(a) carries the mechanism and the callsite.
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

Measured at the campaign-start tree, which is the tree that selected these items:

| phase | inclusive |
|---|---|
| `lower::build_module` | 17.49 % |
| `<AtomTable as Clone>::clone` at `src/codeserver.rs:919` | 15.07 % (298 M) |
| `decode::decode_stream` | 9.00 % |
| dropping the decoded `Vec<RawInstr>` | 4.04 % |

The second row is no longer in the profile: `build_module` now takes a **fork** of the shared table
(`src/codeserver.rs:921`) and appends a literal-only atom to that, so a module load shares the
`Arc<str>` names instead of materializing one per atom — the fork's own paired measurement is
**−276.1 M / −15.78 %** (§ the landed changes, §(a) below).
`Clone` keeps its deliberate deep-copy meaning, which two sibling suites rely on as a positive
control; the fork is the whole-table copy a load may take.
What remains is the load's own shape: the lowering, the code decode and the `Vec<RawInstr>`
teardown.

## Boot collection accounting — 384 collections per boot

One bare boot runs **384 collections**: 145 are **occupancy-driven** (a heap admission found the
nursery full) and 239 are **`collection_debt`-driven** (a grown, never-collected space was retired
by a growth), with a mean of **1 963 free words** at admission, **225 of 384 leaving the capacity
unchanged**, and pid `<0.10.0>` owning **355** of them.
The figures are the valve census's own (`BEAM_RT_VALVE_TRACE=1` over a `run` target that reads
`erlang:statistics(garbage_collection)` — boot plus that one target), which prints the door split,
the `adm/need_pk/cap_mean/avail_mean/occ/debt/...` block per pid and the totals line.

### The mechanism is a shrink/grow oscillation, not the live set

`src/context/calls.rs::hint_grow_gate` → `Process::grow_nursery` retires the space it replaces
(that is `Process::collection_debt`), and the space it installs is the growth table's rounding of a
~172-word copy-out request (**376 words**), so the next admission fills it, collects, and grows it
again — which is the `239` of `384` the census classifies as debt-driven.
`beam_gc::HeapPolicy::young_after_collection` additionally keeps a small nursery: the landed
`MINOR_SHRINK_FLOOR_WORDS` (`../beam_gc/src/policy.rs:197`, the C's `HEAP_SIZE(p) > 3000` gate,
`../../erts/emulator/beam/erl_gc.c:1517-1529`) widens the shrink band to
`max(3 000, 8 × target)`, so a collection no longer hands a capacity at or below **3 000 words**
back.
The observed ladder on one boot is `2 586 → 376 → 986 → 376 → 986`, and the 384 are a **capacity
oscillation**, not the live set asking to be collected.

### The `+hms` sweep — a surrogate for a heap floor (5 paired rounds per row)

| `+hms` | collections | retired instructions | VmHWM |
|---|---|---|---|
| none (that round's control, taken before the floor landed) | 427 | 1 980.5 M | 116.3 MB |
| 1000 | 91 | 1 714.8 M (−13.48 %) | — |
| 3000 | 41 | 1 674.8 M (−15.44 %), cycles −8.8 % | — |
| 8000 | 13 | 1 644.6 M (−16.96 %) | — |

The sweep ran against the binary of its own round, so its `none` row is that round's control: the
percentages are within-sweep paired figures.
It is a **surrogate**, not the landed change: `+hms` raises the *minimum* heap, which suppresses
the same oscillation through a different mechanism, so its **−13.5 % … −17 %** belongs to it alone
and must never be quoted as the minor-shrink floor's value — the floor's own paired measurement is
**−1.2 %** (§(b)).
What the surrogate does establish is the *direction*: measured on the `8000` row it also lowers
peak `VmHWM`, so less copying means less memory here, not more.

## The two cross-node asks — both answered

Both were owned by a sibling crate and were written here as a patch with the value it was expected
to buy; both answers are now in the tree, and each one's own paired measurement is recorded below.

### (a) `beam_term`: `AtomTable::fork(&self) -> AtomTable` — landed

`../beam_term/src/atom.rs:202` is the answer: a whole-table copy that shares the `Arc<str>` name
handles, keeps every index and the `generation`, and takes a **fresh** `instance`, so
`appended_since` is false in both directions between a fork and its parent (a fork diverges by
independent appends, so neither snapshot may validate against the other).
`Clone` keeps its deliberate deep-copy meaning, which is a premise of two sibling suites
(`beam_rt/tests/atom_table_commit_alloc.rs`, `beam_bifs/tests/decode_hot_path.rs`) — they use
`clone()` as a positive control that must scale with the table.
The fork's own evidence is `beam_term`'s unit suite plus `tests/atom_fork.rs` (public-API index and
address identity, the clone sensitivity control, independent appends, the degenerate shapes) and the
allocation pins in `tests/intern_alloc.rs`.
**Value: −276.1 M instructions / −15.78 % of the boot**, measured as 1 749.4 M → 1 473.3 M.
This crate's callsites adopt it: `CodeServer::load_bytes` (`src/codeserver.rs:921`) hands
`lower::build_module` `self.atoms.fork()` and merges the fork's literal-only tail back in index
order, and `src/eval/mod.rs`'s `cook_supplied_trace` takes the same fork, so neither path
materializes a name string per atom.
The figure is the median of six alternating paired rounds on the boot floor, noise band ±0.08 %.

### (b) `beam_gc`: a minor-shrink floor — landed

`../beam_gc/src/policy.rs:197` is the answer: `pub const MINOR_SHRINK_FLOOR_WORDS: usize = 3000`,
the C's `HEAP_SIZE(p) > 3000` gate (`../../erts/emulator/beam/erl_gc.c:1517-1529`).
`HeapPolicy::young_after_collection_with_need` widens the shrink band to
`max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)`, so a collection hands back only a
capacity that is *both* far above the live set **and** above the floor; the `capacity < target`
branch still grows onto the target, and `young_after_fullsweep` deliberately does not apply the
floor (its own rule is gated on `H_MIN_SIZE`).
The rustdoc and the crate's unit suite carry the band, its two branches and the C gate.
**Value: −1.2 % of the boot** (paired, against a from-source control at the base commit) — not the
`+hms` surrogate's −13.5 % … −17 %, which measured a different mechanism (§ the sweep above).
**Blast radius.** The widened band is `(1 864, 3 000]` — the capacities that `8 × 233` no longer
covers but the floor does.
Nothing at or below 1 864 moves: the 233-word nursery floor, 843 (`heap_growth`) and 987
(`reds_boundary`) all stand, as do `bif_extra/process_info_spawned_exact_item_values`'
`{heap_size,233}`/`{total_heap_size,233}`.
The two re-derivations the ask named are settled: `src/runner/tests/boundary_cost.rs`'s
`MEASURED_YOUNG_CAPACITY = 2_585` is still the capacity this tree runs (its row dump reports
`peak_young_capacity` 2 585, and the row's collection totals beside it are the same measured
figure), and `src/context/tests.rs::the_copy_bound_ping_pong_grows_the_nursery_instead_of_declining_the_valve`
is green.
The floor is measurement-neutral for that row because it does not move the edge of the row's own
keep band: at the shape's 376-word minor target the edge is `YOUNG_SHRINK_FACTOR × 376 = 3 008`
either way, so 2 585 stays inside it and neither the capacity nor the collection cadence is the
floor's to change.

## Measurement discipline

* **Count in the slash form, on a P-core.** The host is hybrid (P-cores 0–3), so
  `taskset -c 2` and the **`cpu_core/instructions/u`** event (and `cpu_core/cycles/u`) — never a bare
  `instructions:u`, and never the colon form `cpu_core/instructions:u`, which silently lands on
  `cpu_atom` and drifts ~10×.
* **A stashed binary is never a control** — and neither is a from-source rebuild compared in **wall**
  clock: identical source is instruction-identical but 2–4 % apart in wall, so a single-binary wall
  delta of that size is the host's own spread.
  Rebuild the control from its own source **at the base commit** in a detached worktree and
  alternate the two binaries **inside** every round; quote retired instructions for anything under
  ~3 %.
* **Record the load.** `/proc/loadavg` before and after every round — this host is shared, so a
  comparison without it is not a comparison.
* **The record instrument is [`../../tools/bench`](../../tools/bench)**:
  `run --runtime all --quick --workloads baseline`, with `$BEAM_RT` pointing at an alternate binary.
  It is the sibling tooling node and read-only to this crate.

## See Also

* [`PERF.md`](PERF.md) *Current startup regime* → the whole-process wall/CPU characterisation and the
  boot path's poll-budget note; the tier effect and the one-shot cold-start characterisation.
* [`PERF_HISTORY.md`](PERF_HISTORY.md) → the dated `BEFORE`/`AFTER` snapshots and the
  single-mechanism controlled A/Bs.
* [`HANDOFF.md`](HANDOFF.md) → the cross-node gap ledger; the startup pass's two asks are **not**
  in it — both are answered (§ above).
* [`M1_GAPS_GC.md`](M1_GAPS_GC.md) *GC / heap model* and *Heap switches* → the collection points,
  the valves and the heap policy the second ask landed in.
* [`src/context/gc/CONTEXT.md`](src/context/gc/CONTEXT.md) → the valve walk the first landed change
  made cheaper, and its per-pid census.
* [`src/codeserver/CONTEXT.md`](src/codeserver/CONTEXT.md) → the code store whose load dominates the
  profile.
