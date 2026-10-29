# `beam_rt` — what a `beam_rt` startup costs, and where the cost is

The startup record: the retired-instruction floor of one `beam_rt -noshell -eval 'halt().'`
invocation, the paired delta each landed change bought, the current profile's self-time split, the
boot's collection accounting with its mechanism, and the two cross-node asks — both answered, with
what each answer is and what it is worth.
The instrument is a **retired-instruction count**, never a wall time (the footprint section is the
one exception and names its own instrument): the figures below are
`perf stat -e cpu_core/instructions/u` / `perf record -e cpu_core/instructions/u -c 500000
--call-graph dwarf,8192` over `./target/release/beam_rt -noshell -eval 'halt().'`, pinned
`taskset -c 3` on a P-core, with the two binaries alternated inside every round and each round's
`/proc/loadavg` recorded.
A row that names its own pin and record period does so because that round measured at it.
**Every number here is one host's** (a hybrid-CPU machine whose P-cores are 0–3, shared with other
work): the deterministic counter is the signal, and a wall figure is quoted only where the effect
is far above the host's own spread.

## The floor

| point | retired instructions | wall p50 | ratio vs. the unmodified control |
|---|---|---|---|
| campaign start (the control) | 2 045 799 320 | ≈ 220 ms | 1.00 |
| the atom-table fork's own control | 1 749.4 M | — | 0.855 |
| the previous round's control (commit 2d0fe67808) | 1 083.7 M | — | 0.530 |
| current | 1 073.6 M | — | 0.525 |

The control is the base commit **rebuilt from its own source**, never a stashed binary: a
from-source rebuild of identical source is instruction-identical but 2–4 % apart in wall, so a
kept binary cannot serve as a control — the one exception is a **paired** alternate, which is what
the pre-round binary (`2d0fe67808`) is here: it alternates against the current binary inside every
round, so each round's own ratio is measured back to back.
`−47.5 %` of the boot against the campaign start is what the changes below account for, and each of
their deltas is a **paired** measurement against its own from-source rebuild — which is why each
round's own control (the fork's 1 749.4 M, this round's 1 083.7 M) is a row of its own and is not
the campaign-start figure: they are different rounds of the same tree lineage.
The last round's changes took **1 083.7 M → 1 073.6 M** (**−0.93 %**), the median of 5 interleaved
rounds, `taskset -c 3`, 1-min load 130–137.
The floor's current rows quote no wall figure: the host's own spread on wall is 2–4 %, so retired
instructions are the signal, and a wall figure is quoted only where the effect is far above that
spread (the campaign-start `≈ 220 ms`) or where it is explicitly load-flagged (§ *Instructions are
not time*).
What the floor still holds and no single change owns is the profile below: the interpreter's
dispatch and the module-graph load.

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
(`1304a6b44c`) — the walk is cheaper per call, not absent, though it no longer holds a self-time row
of its own in the profile below.
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

## Instructions are not time — the execution-tier effect

Measured on one binary (the final HEAD `target/release/beam_rt`), 5 interleaved randomized rounds per
program, `taskset -c 3`, 1-min load 130–137, `wait4` per-spawn `ru_utime+ru_stime` and `ru_maxrss`:

| program | retired user instr | cpu_core cycles | task-clock p50 (ms) | CPU p50 (ms) | wall p50 (ms) | peak RSS p50 (MiB) |
|---|---|---|---|---|---|---|
| default (`JitMode::Auto`) | 1 073.6 M | 342.6 M | 117.5 | 119.1 | 153.6 | 48.53 |
| `BEAM_RT_TIER=baseline` | 840.4 M | 385.2 M | 128.7 | 129.1 | 161.5 | 55.58 |
| `BEAM_RT_TIER=interpreter` | 1 073.7 M | 345.1 M | 117.5 | 117.6 | 140.8 | 48.48 |
| pre-round build (`2d0fe67808`) | 1 083.7 M | 452.9 M | 148.3 | 149.8 | 193.5 | 58.84 |
| system oracle (OTP 29.1.2 / erts-17.1.1) | 787.9 M | 344.6 M | 126.6 | 122.7 | 139.7 | 59.45 |

The default boot is byte-for-byte the interpreter boot in cost, because the phase-scoped tier rule
selects the interpreter for the boot phase.
Yet it retires **27.7 % more** user instructions than the pinned-baseline boot while spending
**11.1 % fewer** cycles, **8.7 % less** task-clock, **7.7 % less** CPU and **12.7 % less** peak RSS —
because the baseline tier's compile work (`mmap`/`mprotect`/first-touch page faults on code buffers)
is kernel-side and stall-bound, invisible to `cpu_core/instructions/u` and expensive in
cycles/task-clock.
A retired-instruction count must therefore never be read as a time ratio for a tier comparison.
Against the pre-round build the final boot is **−24.3 %** cycles, **−20.8 %** task-clock, **−20.5 %**
CPU and **−17.5 %** peak RSS on **−0.9 %** instructions.
Wall figures in this table are load-flagged (1-min load 130–137); the load-robust instruments are
`ru_utime+ru_stime`, `cpu_core/cycles/u` and task-clock.

### The measured wall/CPU record at a quieter window

21 interleaved randomized rounds per binary, `taskset -c 3`, 1-min load 48.6–54.0, per-round paired
ratios over the same boot program:

| boot A/B (21 paired rounds) | CPU p50 | wall p50 | peak RSS p50 |
|---|---|---|---|
| new | 116.1 ms | 122.8 ms | 48.51 MiB |
| system oracle | 128.1 ms | 133.7 ms | 59.37 MiB |
| pre-round build (`2d0fe67808`) | 147.2 ms | 156.6 ms | 58.97 MiB |

Paired per-round medians read **new/oracle CPU 0.903, wall 0.910, RSS 0.817** and **pre-round/new
CPU 1.262, wall 1.260, RSS 1.216**.
The module run `run <slice.beam> slice:fib/1 25` (15 interleaved rounds, 1-min load 46.2–48.6) reads
CPU p50 **119.7 / 143.3 / 148.3 ms**, wall p50 **123.6 / 148.9 / 152.2 ms** and peak RSS p50
**48.57 / 64.52 / 59.00 MiB** in new / oracle / pre-round order in every column, with paired medians
new/oracle **CPU 0.831, wall 0.837, RSS 0.753** and pre-round/new **1.240 / 1.240 / 1.214**.
This window is the wall/CPU record; the table above it is the instruction record, and its wall column
is there to show why the two disagree.

## The current profile — self-time

`perf record` over 10 boot invocations, 20K samples, `perf report --no-children --sort symbol`
(self-time), `taskset -c 3`, 1-min load 130–137:

| symbol | self-time |
|---|---|
| `<beam_engine::machine::Machine>::step_instruction_inner` | 23.54 % |
| `<beam_engine::machine::Machine>::drive` | 9.11 % |
| `beam_loader::decode::decode_stream` | 5.94 % |
| `beam_loader::lower::group::group_functions_checked` | 5.27 % |
| `__libc_malloc2` | 4.35 % |
| `<BudgetContext as ExecutionContext>::reduce` | 4.28 % |
| `beam_engine::families::dispatch` | 3.96 % |
| `<beam_term::atom::store::IndexShards>::get` | 2.06 % |
| `<Machine>::eval_test` | 1.96 % |
| `<beam_common::md5::Md5>::hash_part` | 1.82 % |
| `beam_rt::context::gc::alloc_words_rooted` | 1.75 % |
| `_int_free_chunk` | 1.71 % |
| `beam_loader::lower::instr::lower_instr` | 1.65 % |
| `beam_loader::lower::build_module` | 1.64 % |
| `beam_loader::lower::validate::scan_operand` | 1.54 % |
| `drop_glue::<beam_loader::operand::Operand>` | 1.38 % |
| `beam_message::copy::shallow_unshared` | 1.31 % |

The shape is **engine dispatch first** — the boot phase runs on the interpreter, so
`step_instruction_inner` + `drive` + `families::dispatch` + `eval_test` are ≈ **38.6 %** of the boot.
**Module-graph loading is second**: the `beam_loader` symbols sum to ≈ **18.3 %** self-time (decode,
group, lower, operand validation and the raw-operand teardown), with the allocator at ≈ **6.9 %**
(`__libc_malloc2` + `_int_free_chunk` + `_int_malloc`), the atom store's `IndexShards::get` at
**2.1 %** and MD5 sealing at **1.8 %**.

## Where the load cost sits

In the same `perf record` the loader's inclusive share of the boot is **≈ 13–17 %** of the run
(`<CodeServer>::load_bytes` inclusive; the call-graph attribution varies by that much under
`--call-graph dwarf`), with `beam_loader::lower::build_module` ≈ **8 %** inclusive and the
decode/group/validate self-times above making up most of the rest.
`load_bytes` hands `lower::build_module` `self.atoms.fork()` and merges the literal-only tail back,
so a load no longer materializes a name string per atom.
The fork's own historical combined figure — **−276.1 M / −15.78 %** — predates the merge back of the
literal tail and is quotable only as that round's paired delta, never as present-day headroom.

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
The ladder recorded on the boot that census measured is `2 586 → 376 → 986 → 376 → 986`, and the
384 are a **capacity oscillation**, not the live set asking to be collected; on this tree the same
swing carries up to the collapse's **carriage bound** (`max(3 000, 8 × the live set's minor target)`,
`BEAM_RT_GC_TRACE` showing `3 008` as its upper step for a ~238-word live set).

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
Nothing at or below 1 864 moves: the 233-word nursery floor and `heap_growth`'s two settled
capacities — 843 on the rows that never take a fullsweep, 987 after one — all stand, as do
`bif_extra/process_info_spawned_exact_item_values`'
`{heap_size,233}`/`{total_heap_size,233}`.
`reds_boundary`'s 4 185-word plateau is above the 1 864-word edge, so it is not one of these: it is
the collapse's carried minor target, a fixed point inside the keep band `young_minor_target_words(1 201)
= 1 598` up to `8 × 1 598 = 12 784`.
`src/runner/tests/boundary_cost.rs`'s `MEASURED_YOUNG_CAPACITY = 3_000` is the capacity this tree runs
(its row dump reports `peak_young_capacity` 3 000 at every row, and the row's collection totals
beside it are the same measured figure), and
`src/context/tests.rs::the_copy_bound_ping_pong_grows_the_nursery_instead_of_declining_the_valve`
is green.
That row is the floor's own figure: at the shape's 233-word minor target the keep band's edge is
`max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × 233) = max(3 000, 1 864) = 3 000`, so the
carried collapse installs the floor itself and neither the capacity nor the collection cadence sits
below it.

## Where the bytes are — the minimal-run footprint
The measured OS peak (`VmHWM`, `wait4`/`ru_maxrss`) of `beam_rt -noshell -eval 'halt().'` is **48.53 MiB**
5 interleaved randomized rounds against the pinned-baseline build of the same tree and the pre-round build, 1-min load 130–137:
- **Against the two tier controls**: 48.53 MiB is **7.05 MiB below** the 55.58 MiB the identical build measures with the baseline tier pinned for the whole process (`BEAM_RT_TIER=baseline`, which reproduces the pre-phase selection) and **within 0.05 MiB of the 48.48 MiB `BEAM_RT_TIER=interpreter` ceiling** — i.e. the phase rule captures 99.3 % of the interpreter-boot advantage while steady state keeps the native tier.
- **Against the oracle**: a contemporaneous system-oracle boot is 59.45 MiB (same harness), so the default boot peak is **0.817×** and the pre-round build's is 0.993×.
- **Wall/CPU**: 21 interleaved randomized rounds at 1-min load 48.6–54.0 read wall p50 **122.8 ms** (default) / **156.6 ms** (pre-round build) / **133.7 ms** (oracle), CPU p50 116.1 / 147.2 / 128.1 ms — paired per-round medians `new`/`oracle` wall **0.910**, CPU **0.903**, and `old`/`new` wall **1.260**, CPU **1.262**.
- **Every workload row's peak RSS moves with the boot**: three interleaved `bench run --workloads list_walk,copy_bound` pairs per side (load 29–86 recorded) read `list_walk` 61.81 → 50.77 MB and `copy_bound` 63.14 → 52.34 MB (0.821× and 0.829×) with every `case_*_peak_rss_bytes` 0.819–0.829×, because the boot's compiled code is the part those peaks share.
- **Steady-state throughput does not move**: `case_list_walk_elements` is 50 000 000 and every `case_*_ops` counter is identical on both sides (the deterministic work counters); the `ns_per_element` rows (18.2 → 21.5 with the load rising 29 → 86 across the rounds) and every `case_*_wall_ms` row are load-flagged, not the record.
- **The recorded `90.1 MiB` / `128.9 MiB` pair is the `baseline` *compute* run, not `halt().`** — `tools/bench/src/workloads/baseline.rs:144-151` accumulates `peak_memory_bytes` only from the `COMPUTE_EVAL` runs (`:58-59`, `lists:sum(lists:seq(1,1000000))`), while the startup runs (`:119`, `STARTUP_EVAL = "halt()."` at `:71`) record latency only.
- **The compute run's ≈31 MB over the boot is two ≈16 MB copies of the 1M-cons list**: `lists:seq/2` builds it through `ctx.cons` on the pooled BIF heap (`../beam_bifs/src/lists.rs:847-856`), then the copy-out duplicates the container into `eval_heap` (`src/context/pool/CONTEXT.md:80`).

What the peak is made of, and what it is not:
- **The peak is allocator-visible, not a beam region**: massif's peak snapshot is **35.3 MiB heap + 1.6 MiB allocator overhead**, which is exactly the 47 064 kB `rw-p` anonymous bucket (`smaps`, 28 mappings), while both 128 MiB regions sit at 12–48 kB resident (`src/context/pump/CONTEXT.md` *Capacity is not RSS*) because `Heap::new` writes one block header into a lazy `mmap`.

| Peak site (owner) | MiB | What it is |
|---|---|---|
| `<CodeServer>::load_bytes` → `lower::build_module` → `group_functions`'s `Vec::with_capacity<Instr>` (`../beam_loader/src/lower/group.rs:148`) — **beam_loader** | 18.32 | the peak **snapshot's** live bytes at that one allocation site — one exact allocation per function body, so no growth transient sits beside it |
| `beam_term::atom::store::IndexShards::insert` → `Arc::make_mut` → `Rc::clone_from_ref_in` — **beam_term** | 1.25 | the in-flight copy-on-write shard clone at the peak moment (the whole atom-store subtree — both diverged array generations and the shard maps — is 3.06) |
| 549 sites below massif's 1 % threshold (`beam_loader`/`beam_rt`/`beam_bifs`) | 4.27 | no single one ≥ 0.35 MiB |
| `Vec<Operand>` per instruction (`lower::instr::lower_instr` → `emit_fused`) + the `Vec<Block>` and label `BTreeMap`s from `build_blocks` + the one per-image `build_label_map` behind `Module::label_map` (`../beam_loader/src/labels.rs`) — beam_loader / **beam_engine** | 1.33 / 1.15 / 1.18 / 1.05 | per-function operand vectors, block vectors, the image's single label map |

**So ≈24 of the 35.3 MiB is `beam_loader`'s lowering** (its largest single site is the per-function-body instruction-array allocation at 18.32 MiB of the peak snapshot, 5.5 MiB the per-function companions — operand vectors, block vectors, label maps, the `Vec<Function>` at 0.80), **≈3.1 MiB is the atom store's copy-on-write shard generations**, and the remainder is spread over the 549 sites below massif's 1 % threshold; what the boot's peak is made of is therefore a *retention* question about the module graph — a `beam_loader`/`beam_term` matter.
- **The dominant consumer is retained module IR**: `size_of::<Instr>() == 64` at ≈4.25 `Code`-stream bytes per instruction ⇒ ≈15 B of IR per `Code` byte, ≈3.5× a module's `.beam` image (`../beam_loader/CONTEXT.md:61-63`), and `-eval`'s `erlang:memory(total)` is only 7.71 MB because this heap is outside the `MemoryHook`'s accounting (`src/context/memory_hook.rs:40-58`, `../../tools/bench/RESULTS.md`).
- **`erlang:memory(atom)` counts the *shared* table only** — `8 × atoms.len()` index slots plus Σ name bytes (`src/context/memory_hook.rs:202-210`, wired at `:162`, `atom_used = atom`) — so the per-module `AtomTable` forks are invisible to it; a third fork site is the synthetic module `cook_supplied_trace` builds (`src/eval/mod.rs:1927`).
- **Processes, not reservations, are what the heap counts**: each live process costs one `Machine` (written at construction) + a `Process` PCB (~9.3 KiB, its `XRegs`) + a 233+233-word heap inside a shared 1 MiB carrier — there is no per-process `beam_alloc::Heap` reservation, and nothing pins how many processes a boot leaves live (`src/boot/CONTEXT.md:267-276` enumerates them, `src/runtime.rs:5419` pins only `Runtime::new`'s four).
- The VM's own accounting (`vm_memory_total_bytes`) is only 7.7 MB, so the figure is **boot-dominated** and mostly outside accounted memory; `-version`, which boots nothing, is 4.8 MiB against the oracle's 11.9, so the peak is boot work, not the process image.
- The two 128 MiB term-construction heaps (`DEFAULT_HEAP_WORDS`, `src/config.rs:68` → `bif_heaps` and `eval_heap`, `src/runtime.rs:1564`/`:1578`) are **address space, not RSS**: `beam_alloc` maps `MAP_PRIVATE|MAP_ANONYMOUS` with no populate (`../beam_alloc/src/mseg/mapping.rs:72-91`) and `Heap::new` does not zero (`../beam_alloc/src/heap.rs:59-74`), so untouched pages never become resident (12–48 kB measured) — and no `MALLOC_MMAP_THRESHOLD_`/`TOP_PAD_`/`ARENA_MAX`/`TRIM_THRESHOLD_` setting moves the peak.
- Per-process collector capacity is 233 nursery + 233 mature words ≈ 3.7 KiB (the default spawn policy, `src/config.rs:124-132` → `../beam_gc/src/policy.rs:38,337-347`), so even the ~33–38 live processes at the halt instant account for ≈140 KiB.
- **One `Machine` per spawned process** (`src/runner/entry.rs:514`; ~26 KiB fixed: 1 024-word `x` + 1 024-word `fr` files, 256-word stack, 256 frames, 8 catch frames) plus one **transient** machine per boot-script `{apply,…}` (`src/boot/drive.rs:370`, dropped at `:426`); the four `Runtime::new` services get none, because they are never enqueued. Nested runs recycle through the thread-local pool (`src/context/machine_pool.rs:182-185`, ≤ 32 idle machines), which starts empty.
- **Only 7 of the 22 boot directives enter `.beam` code** (3 `{kernelProcess,…}`, 4 `{apply,…}`) and **nothing compiles during boot** — under `JitMode::Auto` every first entry selects `Tier::Interpreter` while `Runtime::in_boot_phase()` holds, so the whole boot window runs the reference tier (`src/runner/tier.rs`; the per-directive map is `src/boot/CONTEXT.md` *Boot-path engine-entry map*).
- The engine arena (`arena: MinimalContext`, `src/runtime.rs:1571`) allocates **nothing** until the first `alloc_words`; the first non-zero request reserves 4 MiB and commits 64 KiB (`../beam_engine/src/context.rs:1438,1444`).
`tests/rss_bound.rs:39-40` bounds the boot peak at 120 MiB — a ceiling with ~71 MiB of headroom over the 48.53 MiB default.

## The 99 boot-loaded modules and what they retain (measured against the host oracle)
A bare boot opens **99 distinct `.beam` files totalling 5.80 MiB**: the 22 preloaded modules and the script's pre-marker `{primLoad, …}` batch of 29 names are only the **eager** part — 3 016 872 B (2.88 MiB), the largest of them `erl_parse` 707 236 B (691 KiB), then `erl_lint` 289 356, `erlang` 249 592, `erl_eval` 125 520, `lists` 124 144 — and the other 48 open during the same boot, on demand (`src/boot/preloaded.rs`, `src/boot/bootscript.rs`; the post-marker batches of 86 + 87 names are `init`'s no-ops).
Retained after the load, per `Module` (`../beam_loader/src/ir.rs:282`), with the consumer that justifies it (the sizes below are those of the 51 eager modules; the other 48 add to every row):

| Retained item | Size (51 eager modules) | Consumed later? | By whom |
|---|---|---|---|
| `Function::code` (`Vec<Instr>`, 64 B, `../beam_loader/src/instr.rs:186`) | 10 455 808 B ≈ 9.97 MiB (163 372 `beam_disasm` instructions — the raw, pre-fusion count) | yes | every tier (`beam_engine/src/jit/…`, `interpreter`) |
| `Block` vecs (24 B) | 744 960 B ≈ 727 KiB (31 040 blocks) | yes | the JIT's block linker |
| `exports`/`imports`/`funs`/`records`/`types`/`line` | ≈ 20 916 + 25 476 + 9 984 + 4 637 + 37 297 B ≈ 98 KiB | yes (`records`/`types`/`line` also feed the JIT cache key) | `families/funs`, `jit/cache/key.rs`, `beam_engine` trace |
| `literals: Arc<LiteralArea>` | 28 664 decoded words = 223.9 KiB live inside a `4×chunk+64`-word reservation (1.7 MiB address space) | yes | `literal` op / its `Eterm` operands |
| raw `attributes` (`Attr`) | 81 592 B chunk ⇒ ≈ 70.5 KiB stripped (`erl_parse` 74 886 B) | `context/module_info.rs` + hashed at `jit/cache/key.rs` | `get_module_info(M,attributes)` |
| raw `compile_info` (`CInf`) | 15 669 B | `context/module_info.rs` + `jit/cache/key.rs` | `get_module_info(M,compile)` |
| `strings` (`StrT`) | 61 B total | yes | `bs_match_string`/`bs_create_bin` string segments (`families/bitstrings/…`) |
| `atoms: Vec<Atom>` + `atom_table` fork | 9 501 atoms / 107 190 name bytes; the fork is `Arc` chunk/user-shard clones (zero bytes) | yes | `ExecutionContext::atoms` |
| `checksum: [u8;16]` | 16 B | yes (JIT cache digest) | `jit/cache/key.rs` |

`Meta` is the one stored chunk with **no production reader** (`Module::meta` is written at load and read only by `beam_loader`'s own tests); every other retained field is a row above or image bookkeeping (`name`, `code_version`, the `label_map` slot).
**Retained IR is therefore ≈ 20 MiB over the 99 modules** (`../beam_loader/CONTEXT.md`'s independent census reads the same set at ≈ 304 000 instructions × 64 B ≈ 19.2–19.4 MB), **70 of the 99 execute at least once** in a bare boot (≈ 14.3 MiB of that IR) while **29 never execute** — ≈ 1 719 KiB of `.beam` ⇒ **≈ 5.8 MiB of dead IR** — namely `erl_parse`, `erl_lint`, `erlang`, `net_kernel`, `erl_prim_loader`, `prim_socket`, `erts_internal`, `prim_file`, and the `atoms`/`counters`/`persistent_term`/`zlib`/`erl_tracer`/`erts_*`/`prim_*`/`socket_registry`/`error_handler`/`error_logger`/`logger_backend`/`logger_filters`/`heart`/`erl_init`/`os` tail.
The census is an **upper bound**, not a weighing: its instruction count is `beam_disasm`'s raw pre-fusion count and `size_of::<Instr>() == 64` is a policy constant, while fusion only lowers the retained `Vec<Instr>`.
**Massif's peak snapshot lands on the same site** — ≈ 19 MB (50.41 % of the boot heap) in the loader's per-body `Vec::with_capacity<Instr>` at `load_bytes → lower::build_module → group_functions` (`../beam_loader/src/lower/group.rs`), one exact allocation per body, so no growth transient sits beside it (§ *Where the bytes are* above); `read_image`'s `std::fs::read` buffer is dropped (`src/codeserver.rs:821`), so no `.beam` image is retained.
`get_module_info(M, attributes|compile)` does **not** read a decoded table — it re-decodes the raw chunk through a fresh 8 192-word scratch `Heap` per call (`src/context/module_info.rs:281-289`).
The per-module `AtomTable` fork is re-taken from the **merged** shared table at the end of the load, so the module pins no diverged shard copies of its own — the copies its literal appends made are dropped at load (`src/codeserver.rs:949`, `../beam_loader/CONTEXT.md:65`).
The loader's side of that pricing — the IR ratio and the per-module literal arena — is `../beam_loader/CONTEXT.md`'s; `src/codeserver/CONTEXT.md` *Cost shape* holds the code-path derivation's cost.
Per-route detail: [`src/boot/CONTEXT.md`](src/boot/CONTEXT.md), [`src/context/CONTEXT.md`](src/context/CONTEXT.md), [`src/runner/CONTEXT.md`](src/runner/CONTEXT.md), [`src/eval/CONTEXT.md`](src/eval/CONTEXT.md); the store/arena ceilings in [`src/context/pump/CONTEXT.md`](src/context/pump/CONTEXT.md).

## Measurement discipline

* **Count in the slash form, on a P-core.** The host is hybrid (P-cores 0–3), so
  `taskset -c 3` and the **`cpu_core/instructions/u`** event (and `cpu_core/cycles/u`) — never a bare
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
