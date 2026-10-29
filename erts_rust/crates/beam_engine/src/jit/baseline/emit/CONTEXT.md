# `emit` -- Baseline JIT x86-64 Instruction Emitters

## Intent
Lower the typed IR basic blocks of a `beam_loader::Module` into native x86-64 functions for the
**baseline** execution tier.
One file per responsibility: `mod.rs` owns the shared emitter wiring (the register-file/scratch
constants, `touch`, the spill/fill traffic, the helper-sequencing layer every arm shares —
`emit_helper_call`/`emit_step_and_continue`/`emit_continue_check`/`emit_refresh_y` — plus the
re-exports); `block.rs` owns the compiled-block shape (the `emit_block` lowering dispatch, its
helper-sequence arms and its epilogue/deopt wrapper) and `prologue.rs` the block prologue it emits
first (`emit_block_prologue` and its position-store helpers); `regs.rs` owns the register cache
(`Allocation`/`spill`/`fill`/`SpillSite`/`FillSite`); `compile.rs` owns the compilation entry points
— `compile_module` (the **eager** whole-module layout: one shared `CodeBuffer`, a whole-module block
map) and `compile_function`/`emit_function` (the **lazy** layout's per-function unit: its own
`CodeBuffer`, a map covering only that function) — plus the continuation sub-blocks and the code-size
estimates; `regalloc.rs` picks the cached x-registers and derives each block's Y/refresh gates;
`liveness.rs` computes the per-pc fill sets; `operands.rs` reads operand shapes and owns the shared
guarded-Y access plus the `Loc` location abstraction; `r#move.rs`/`test.rs`/`select.rs`/`arith.rs`/
`compare.rs`/`aggregate.rs`/`bif.rs` each lower one instruction group (a `bif.rs` accessor that is a
reused aggregate read emits that read's own bytes; the helper-evaluated value accessors emit a plain
helper call); `frame.rs` lowers the frame-discipline
trio that ends a function body (`deallocate`/`return`/`deallocate_return`); `heap.rs` lowers the heap
/ frame-admission family (`allocate`/`allocate_heap`/`test_heap`/`trim`); `family.rs` emits the shared
helper arm for every instruction the interpreter routes to `crate::families::dispatch`; `link.rs`
owns the direct block-to-block jump, the linked local tail call, the unlinked tail-call exit and the
in-block transfer of a non-tail `call` whose callee block is not part of this compilation.
The driver, block ABI and the helpers (`jit_step_one`, `jit_ext_call`, `jit_local_call`,
`jit_call_store`, `jit_call_store_linked`, `jit_tail_call`, `jit_deallocate`, `jit_return`,
`jit_deallocate_return`,
`jit_allocate`, `jit_allocate_heap`, `jit_test_heap`, `jit_trim`, `jit_receive`, `jit_family`,
`jit_publish_y`, `jit_refresh_y`, and the inline position store `emit_position_store`) live one level up in `../mod.rs`,
`../runtime.rs`, `../call.rs`, `../receive.rs` and `../family.rs`; this node only *emits calls* to
them.

## Core model (do not break)
- **Only `CACHE_REGISTERS.len()` = 5 x-registers are held in machine registers** (`r12`-`r15` + `rbp`,
  `mod.rs` `CACHE_REGISTERS`); every other x-register an instruction names is addressed **directly in
  `machine.x`** via `[rbx + x*8]` (`rbx` = `rt->x_regs`, loaded in the prologue).
  A register is either *always* cached in its machine register or *never* cached, fixed for the whole
  block, so the two representations can never disagree.
- **Both directions of the register cache are partial.** `spill(buffer, allocation, site, pc)` stores
  exactly `Allocation::spill_slots_at(pc)` — the cached x-registers **dirty at that pc**
  (`Allocation::spill_sets`, the `regalloc.rs` forward dirty walk), i.e. the ones an inline lowering has
  diverged from their `machine.x` slot since the last syncing event, and falls back to the whole
  `Allocation::written` set when the table is empty; `fill` reloads only
  `Allocation::fill_slots(pc)` — the registers the continuation can still observe, the block-local
  liveness of `liveness.rs::fill_sets` unioned with the write set, `(live ∪ written) ∩ slots` — while the
  prologue load is the block's **live-in** slot set (`Allocation::entry_fill`), so a cached register the
  block writes before it reads is never loaded at entry (an absent table falls back to the whole cache).
  The cache itself lives in `regs.rs` (`Allocation`/`spill`/`fill`); the call sites are in `mod.rs`
  (`emit_helper_call`) and the block/link emitters.
- Caller-saved scratch registers, used only *within* one instruction's lowering: `MOVE_SCRATCH`=`r11`,
  `MOVE_SCRATCH_2`=`r10` (`mod.rs`); `Y_SCRATCH`=`r9` and `SWAP_END`=`rax` (`r#move.rs`);
  `FAST_A`=`rax`, `FAST_B`=`r8`, `FAST_C`=`rcx`, `FAST_T`=`r10` (`arith.rs`);
  `TEST_A`=`rax`, `TEST_B`=`r8`, `TEST_C`=`r9`, `TEST_T`=`r10`, `TEST_H`=`r11` (`test.rs`);
  `GUARD_SCRATCH`=`r8` (`aggregate.rs`); `Y_ADDR_SCRATCH`=`r9` / `Y_ADDR_SCRATCH_2`=`r8`
  (`operands.rs`, the pre-computed Y destination addresses for a multi-destination writer).
  None overlaps a cache register, `rbx` or `rsp`.

## The `Move` literal source and the literal-area guard
A `Move` whose source is a **literal-area term** (`MoveValue::Literal`, an index into the running
module's decoded literal area) is lowered by `emit_move_literal`/`emit_guarded_literal_load`: the emitted
sequence loads `JitRuntime::literals_base` and `JitRuntime::literals_end` and checks
`literals_base + index*8 < literals_end` **at run time**, then reads the word.
The guard is deliberately a run-time check against `literals_end`, never a baked pointer, because a cached
block can outlive the `Arc<Module>` it was compiled for and the literal area is re-bound per module
context (`JitRuntime::refresh_literals`, called by the driver before the run loop and after every
cross-module switch). A rejection branches to the helper, whose interpreter step reads the literal area
itself.

## Compile-time tables the emitter builds (what is keyed by what)
Nothing the per-instruction analysis produces is retained in the artifact: an `Allocation` exists only for
the duration of one `emit_block` call (borrowed, then dropped — `CodeBuffer`/`CompiledModule` hold none).
The only tables that outlive a compile are `compile.rs`'s entry tables and the `LabelMap`.
- **`Allocation` (`regs.rs:104`)** — the one per-block table set; every table is a `Vec`-indexed by
  `pc - block_start`: `spill_sets: Vec<Vec<u32>>` (dirty set per pc, `[start, end]` **including** the exit
  pc `end`), `fill_sets: Vec<Vec<u32>>` (post-helper refill per pc, `[start, end)`),
  `entry_fill: Option<Vec<u32>>` (live-in set); plus the scalars `slots: Vec<(u32, Reg)>`,
  `written: Vec<u32>` (whole-block union = the conservative fallback), `block_start: u32` and
  `y_last`/`y_first`/`frame_republish_first: Option<u32>`. An **empty** table means "no analysis"
  (a synthetic allocation) and degenerates to the whole cache/written set.
  Lookups: `spill_slots_at(pc)`/`fill_slots(pc)` (`regs.rs:232`/`:255`) each **allocate a fresh
  `Vec<(u32, Reg)>` per call** (spill and fill sites are hot; the fan-out is ≤ `CACHE_REGISTERS`), and
  `slot(x)`/`is_written(x)` (`:204`/`:212`) are **linear scans** over the ≤5-entry `slots`/`written`,
  not lookup tables.
- **Re-derived, never cached:** `cacheable_touches`/`cacheable_reads`/`cacheable_writes`/`is_inlined`
  (`regalloc.rs:523`/`:666`/`:780`/`:422`) and the `uses_y` union (`arith.rs:361` + `bif.rs:323`) re-run
  the shape predicates per instruction per call — there is no per-instruction verdict table. `regalloc`
  runs `uses_y` **twice** over the block (once for `y_last`'s max, once for `y_first`'s min) and
  `allocate_registers_with_labels` walks the block once for touches/writes, once for `written_set`, then
  once for the fill walk and once for the dirty walk.
- **Every continuation sub-block pays its own analysis** (`compile.rs:342` and `:450`): the range
  `[start, end)` gets a fresh `allocate_registers_with_labels`, because `Allocation` is
  entry-point-relative (`block_start` + the two per-pc tables + `entry_fill`).
- **Keyed by `(func, pc)`, two different structures with the same key** (`compile.rs`):
  `block_labels: BTreeMap<(u32, u32), CodeLabel>` (`:86` eager / `:179` lazy — a **label** per block
  start, so a forward branch is an ordinary `rel32` fixup) and the driver-facing entry tables
  `entries`/`sub_entries` (`Vec<Vec<*const u8>>` for the eager layout's `[func][pc]`, `Vec<*const u8>`
  for a lazy function's `[pc]`, each slot null when that address compiled no block — **flat
  indexing**, chosen over a `HashMap` because the driver probes it on every cross-block jump).
  The **last** step of each layout, after `emit_sub_blocks`/`emit_shared_sub_blocks` and before
  `buffer.seal()`, folds the two into one: `merge_sub_entries` null-fills each block-start table with
  the continuation sub-block entry at the same pc (a pc is at most one of the two, and a block start
  still wins), so the published table answers both shapes with one lookup. Addresses do not move —
  the fold runs after every label is bound and only rewrites slot contents.
  `Linker` (`link.rs:229`) holds only `&BTreeMap<(u32, u32), CodeLabel>`; it owns no fixup table — the
  `rel32` patching is the `CodeBuffer`'s own label machinery.
- **`LabelMap`** (`../../interpreter/mod.rs:88`) is the one *pre-decoded* table stored **in** the
  artifact (`code.rs` `CompiledModule::labels`): dense `Vec<u64>` indexed by **label id** (packed
  `func << 32 | code_index`) or a sorted sparse `Vec<(u32, (u32, u32))>`, plus `imports: Vec<..>` indexed
  by `ImportRef.0` (each import's names resolved once). Not pc-keyed.
- **Sub-block discovery is linear, not indexed** (`compile.rs:244` `sub_block_end`, `:311`/`:415`): per
  continuation pc the code does `blocks.iter().find(..)` plus a fresh scan for the next
  `stores_continuation` pc, i.e. O(blocks · continuation-sites) with no precomputed pc→block index.

## Where the detail lives (this node's children)
This file is the *contract*: the core model, the literal-area guard, the constraints, the constraints' register/budget rules, the routing table and the known issues.
The rest is one level down, each child owning one mechanism:
- `shapes/CONTEXT.md` → the lowering dispatch table, every restricted shape, the three deopt levels and the heap / frame-admission family.
- `prologue/CONTEXT.md` → the compiled-block prologue, the position store / Y publish, and the spill/fill discipline (`ExitSpill`'s single-store rule is `link/`'s).
- `regalloc/CONTEXT.md` → the register picker and the Y/refresh/dirty-set gate derivation.
- `link/CONTEXT.md` → the exits, the `CALL_STORED` in-block transfer, the tail forms' baked probe, and the continuation sub-blocks.
- `regs/CONTEXT.md` → the register cache itself (`Allocation`/`spill`/`fill`/`SpillSite`/`FillSite`) and its store/fill models; the only owner of that contract.
- `test/CONTEXT.md` → the `test`-family shapes, the comparison helper and the refusal census.
- `bitstr_inline/CONTEXT.md` → the native lowerings of the sixteen `Bs*` arms / seventeen shapes (fifteen match-context shapes plus the `bs_create_bin` construction arm's private-append build and bare build), their shared emitters, the new shape modules and the per-shape counters.

## Constraints
- `#![allow(unsafe_code)]` at the emitter entry (`mod.rs`) because it drives the encoder, and the
  attribute covers every submodule of this node; nothing here dereferences a raw pointer itself. The
  helper-calling submodules (`frame.rs`, `heap.rs`, `family.rs`, `link.rs`) are in that same JIT zone:
  they emit bytes and form helper function pointers, never dereferencing a raw pointer themselves.
- `emit_block` accumulates `highest_native_register` (direct `machine.x` accesses) via `touch`; the driver
  refuses native entry to a machine whose register file does not cover the bound the artifact's layout
  requires. `emit_function` records `highest_cached_register` / `cached_registers` and
  `highest_native_register` for the function it lowers; `compile_module` merges those per-function
  summaries into the whole-module summary, and that **whole-module** `highest_native_register` is the
  eager layout's entry guard (its seal-time links may tail-call across functions in the one shared
  mapping). A lazy artifact keeps each function's own summary and guards entry by the entered function's
  own bound.
- `CodeBuffer` reservations are budgeted by `code_capacity_estimate` (whole module) / `function_capacity_estimate`
  (one function, the base of the lazy layout's buffer size). Both are loose on purpose: under-estimating
  turns a large module into a `SystemLimit` compile error. Block entry labels are reserved **before**
  emitting, so forward branches are ordinary `rel32` fixups, never post-`seal` patching (W^X unchanged).
- A resolved `CallExt`/`CallExtOnly`/`CallExtLast` surfaces from the `emit_ext_call` helper as the `ENTER`
  word with the resolved `(context, func, pc)` in the runtime's `enter` slot; the **driver** performs the
  switch (`Machine::switch_context` + module/label/literal re-bind + the callee's blocks via
  `NativeCode::module_blocks`) and re-enters native code. The transfer is a driver round-trip (a
  cross-module link is unsound — separate mappings, buffer-local `rel32` fixups, a module baked into
  `JitRuntime`), but the *call instruction* itself costs no interpreter step; a seam **miss** (the host's
  decline, i.e. the synchronous `call_bif` path) or a failed frame discipline falls back inside the
  helper — the decline through `jit_step_ext` with the answer the helper already computed (so the host
  seam is consulted **once** per call, never re-asked by the interpreter's `call_ext*` arm), an
  unresolvable import/frame discipline through `jit_step_one`.
- Never hand-edit generated data. Apache-2.0 header on every `.rs` (byte-exact, license scanner
  enforced).

## Test Strategy
The dirty-set store set is pinned by the `#[cfg(test)] #[path]` submodule `spill_tests`
(`spill_tests.rs`, declared from `mod.rs`): a write followed by a helper stores the dirty register once,
an inlined instruction that writes nothing does not reset the set, a fully-deopted one does reset it, an
empty `spill_sets` table falls back to the whole `written` set, and a corpus consistency pin checks
`regalloc.rs::is_inlined` against the emitter's own compile-time inlining decision.
`../counters.rs` unit test `one_run_yields_both_the_written_and_the_full_cache_spill_counts` pins that a
4-slot cache with a 2-element write set at two spill sites yields **4** written-set stores and **8**
full-cache stores from one recorded run. The emitted bytes themselves are pinned by
`baseline/tests.rs` (`every_exit_spills_the_cached_registers_before_leaving_the_frame`,
`a_tail_exit_spills_pops_the_frame_and_then_jumps_to_the_successor`,
`a_deallocate_spills_before_calling_the_helper`), which assert against the dirty-set store count.
The `ExitSpill` mode is pinned twice: `link.rs`'s
`a_done_exit_emits_exactly_the_spill_it_skips_less` (the `Required`-vs-`Done` byte-length equality on
all three exits) and `../tail_exit_tests.rs::the_probed_local_call_records_one_spill_call_and_none_at_the_exit`
(per probed tail shape: exactly **one** `SpillSite::Helper` spill call, **zero** `SpillSite::Exit`
calls/stores, and the instruction lowered rather than deopted, so the pin cannot pass vacuously); the
tail forms' interpreter agreement under the probe is
`../tail_exit_tests.rs::the_probed_tail_call_still_agrees_with_the_interpreter`. The corpus-wide
traffic behind the figures quoted above is `../census.rs::
the_register_cache_traffic_census_over_the_corpus_is_printed`, and the per-kind sizes
`../census.rs::the_per_kind_emitted_code_size_table_is_printed`.
Tier-equivalence is proven by the crate's `tests/jit*.rs` real-module gates (interpreter-identical
by construction).


## Routing Table
- `mod.rs` -> the shared emitter wiring: the register-file/scratch constants (`SAVED_REGISTERS`
  — the canonical full callee-saved set — `CACHE_REGISTERS`/`RUNTIME_SLOT_DISP`/`MOVE_SCRATCH`/
  `MOVE_SCRATCH_2`), the variable-frame helpers `native_frame_regs`/`frame_slot_len`, `touch`,
  `emit_refresh_y`, `emit_helper_call` (which takes the instruction's own `pc` as the store pc),
  `emit_positioned_helper_call` (the same sequence **plus** the bitstring arm's inline position store —
  see `../CONTEXT.md`), the private `emit_helper_invoke` both share, `emit_step_and_continue`,
  `emit_continue_check`, `emit_continue_check_helper_refresh`; re-exports
  `regs::{Allocation, SpillSite, FillSite}` and `pub(crate) use block::emit_block`, `compile_module`/
  `compile_function` (keeping every item path stable for `baseline/mod.rs`, `baseline/code.rs`, the
  sibling emitter files and the `#[cfg(test)]` names `baseline/tests.rs` reaches through `emit::*`), plus
  the `#[path]` submodule `spill_tests` (`spill_tests.rs`; see `## Test Strategy`).
- `block.rs` -> the per-instruction lowering dispatch and the compiled-block shape it emits:
  `emit_block` (the `match instr` dispatch), `try_inline_init_yregs`, `emit_deopt`,
  `emit_local_call_probe`, `emit_ext_call`, `emit_receive`; the block prologue it emits first is
  `prologue.rs`.
- `prologue.rs` -> the compiled-block prologue: `emit_block_prologue` (the frame-register pushes, the
  `rt`-slot reservation, the `machine.x` base load, the position store / Y-window publish and the
  live-in cache load) plus the `pub(super)` `emit_position_store`/private `emit_publish_y`/
  `emit_prologue_helper` it uses (`emit_position_store`'s second user is `family.rs`'s bitstring arm).
- `regs.rs` -> the block register cache: `Allocation` (`slots`/`written`/`spill_sets`/`y_last`/
  `y_first`/`frame_republish_first`/`block_start`/`fill_sets`/`entry_fill`,
  `slot`/`is_written`/`spill_slots`/`spill_slots_at`/`fill_slots`/`entry_loads`/`refresh_y_after`/
  `prologue_publishes_y`), `spill` (stores `spill_slots_at(pc)`, falling back to `written`),
  `fill` (loads only `fill_slots(pc)` at a helper site, and the prologue's `entry_fill` set at entry), and the
  test-only `SpillSite`/`FillSite` classification of the call sites; `liveness.rs` computes the per-pc
  `fill_sets` **and** the block's live-in `entry_fill` set, `regalloc.rs` the per-pc `spill_sets`.
- `compile.rs` -> the compilation entry points and what they share: `compile_module` (**eager**:
  whole-module block map + the merged whole-module summary that guards entry, plus `emit_shared_sub_blocks`)
  and `compile_function` (**lazy**: one function's own buffer/entries/sub_entries/summary, sharing
  `emit_function` and running `emit_sub_blocks`), `stores_continuation`, `emit_sub_blocks`/
  `emit_shared_sub_blocks`, `emit_function`, and the code-size budgets (`BLOCK_BYTES`/`INSTRUCTION_BYTES`/
  `FAST_PATH_BYTES`/`COMPARE_PATH_BYTES`/`LIST_PATH_BYTES`/`TUPLE_PATH_BYTES`) with their estimates
  `code_capacity_estimate`/`function_capacity_estimate`/`sub_blocks_capacity_estimate`.
- `regalloc.rs` -> `allocate_registers_with_labels` (the production entry: it takes the module-wide
  `LabelMap` the compilation builds once and threads through `emit_function`/`emit_sub_blocks`/
  `emit_shared_sub_blocks`, so no IR block, function or continuation sub-block rebuilds it) and its
  build-on-demand twin `allocate_registers` (the in-crate unit tests' entry, which builds the map itself
  and delegates) — incl. the `y_last`/`y_first`/`frame_republish_first`, `written`
  and `spill_sets` derivations +
  `cacheable_touches`
  (the per-instruction native touches) + `cacheable_writes` (the block's written x-registers, mirroring each lowering's shape
  test but returning destinations) + `spill_sets`/`is_inlined`/`test_helper_inlined` (the per-pc
  dirty-set walk and its inlining mirror).
- `operands.rs` -> `x_offset`, `x_register`, `y_offset`, `y_register`, `y_addressable`, `constant_word`
  (a thin delegation to the shared definition, `crate::interpreter::constant_word`),
  `small_word`, the shared guarded-Y access (`emit_guarded_y_address`/`emit_guarded_y_load`/
  `emit_guarded_y_store`/`guard_y_store`), the `Loc` location abstraction
  (`of`/`is_y`/`register`/`load`/`guard_store`/`store_at`), `locate_all`, and the
  `Y_ADDR_SCRATCH`/`Y_ADDR_SCRATCH_2` address scratches.
- `r#move.rs` -> `try_inline_move`, `try_inline_swap` (`Move` over in-bound x-registers, an addressable
  `Y` slot on either side behind the frame-window guard, a constant word or a **literal-area term**
  behind the literal guard; `swap` over two `Loc` operands via `emit_swap_y`), `MoveShape`/`move_shape`/
  `MoveValue`/`MoveDest`/`MoveSource`, `SwapShape`/`swap_shape`, `emit_move_literal`/
  `emit_guarded_literal_load` (the literal-area guard), and the `Y_SCRATCH`/`SWAP_END` scratches.
- `test.rs` -> the word-level `test` subset: the mask/tag constants, `TestSrc`/`TestPred`/`TestPlan`/
  `Guard`, `test_plan`, `test_src`, `is_runtime`, `compare_shape`, `arith_guard`, `order_guard`,
  `load_test_src`, `guard_small`, `arity_operand`, `try_inline_test` (incl. its `emit_test_helper`
  comparison fallback), `emit_test_helper` (also `TEST_A`/`TEST_T` reused by `select.rs`).
- `test/header.rs` -> the boxed-term header predicate emitters (`emit_is_integer`/`_number`/`_float`/
  `_binary`/`_arity`/`_fun_arity`/`_tagged_tuple`/`_pid`/`_port`/`_reference`) + the shared
  `load_boxed_header`/`header_matches`/`jump_if_small`/`jump_if_immediate` helpers; it reads the masks
  from `test.rs` via `use super::{…}` (they are not duplicated).
- `test/tests.rs` -> the module's inline unit tests (`#[cfg(test)] mod tests;` in `test.rs`).
- `test/refusal_census.rs` -> the test-only refusal attribution.
- `select.rs` -> `SelectPlan`, `select_val_plan`, `try_inline_select_val`.
- `arith.rs` -> `SmallArith` (`{Add,Sub,Mul,And,Or,Bsr}`), `fast_arith`, `FastArith`/`FastDest`/
  `FastSource`, `FastArith::shift`, `SHIFT_COUNT_LIMIT`, `integer_value`, `uses_y`, `import_names` (the
  shared import→name resolver, also reused by `../bif_helper.rs`), `try_inline_gc_bif`,
  `emit_y_load`/`emit_y_store` (+ the small-int constants).
- `compare.rs` -> `ComparisonOp`, `comparison_shape`, `cmp_operand`, `comparison_guard`,
  `try_inline_comparison`.
- `bif.rs` -> the named-BIF fast paths: `Target`/`Inlined`, `classify`, `element_index`, `value_shape`/
  `value_registers`, the shape predicates `bif_shape`/`bif_uses_y`, the entry `try_inline_bif` and the
  in-block accessors `try_inline_tuple_size`, plus `ARITY_SHIFT` (the `arity << TAG_IMMED1_SIZE`
  derivation from a tuple header). The helper-evaluated `Value` route emits
  `emit_helper_call(jit_bif_value)` + `emit_continue_check`, so it needs no reference-counted operand
  model of its own.
- `aggregate.rs` -> the tuple read / arity switch / tuple write: `TupleReadShape`/
  `get_tuple_element_shape`/`emit_tuple_read`, `TupleWriteShape`/`TupleWriteValue`/
  `set_tuple_element_shape`/`emit_tuple_write`/`try_inline_set_tuple_element`,
  `AritySwitchPlan`/`select_tuple_arity_plan`/`try_inline_select_tuple_arity`,
  `emit_tuple_header_guard`; it re-exports the cons-cell readers from `aggregate/list.rs` via
  `pub(super) use list::{…}` so `aggregate::get_*_shape`/`try_inline_get_*` keep resolving.
- `aggregate/list.rs` -> the cons-cell/list readers: `NOT_LIST_MASK`/`POINTER_MASK`/`CAR_DISP`/
  `CDR_DISP`, `Half`, `ListShape` + impl, `get_list_shape`/`get_hd_shape`/`get_tl_shape`, `shape`,
  `try_inline_get_list`/`try_inline_get_hd`/`try_inline_get_tl`, `emit_list_read`.
- `link.rs` -> `Linker` (`is_compiled`, `emit_exit`, `emit_tail_exit`, `emit_tail_exit_to_driver` —
  each taking a trailing `store_pc` separate from the branch target), `emit_in_block_call` (the
  frame-preserving `call`'s in-block transfer: the `RUNTIME_RESUME_DISP` probe, frame teardown, `jmp r11`,
  and the declined arm's pre-existing `encode_target` word), `emit_frame_teardown`, and the
  byte-level linker unit tests (alongside `test.rs`'s own unit tests).
- `frame.rs` -> `emit_deallocate`, `emit_return`, `emit_deallocate_return` (whose `position_pc` — the
  helper's recorded position — is separate from its `store_pc`, the pair's first pc), `emit_resume_or_exit`
  (the three end-of-body lowerings: the helper's word is tested for a pending in-block continuation —
  `mov rdi,[rsp+RUNTIME_SLOT_DISP]` / `mov r11,[rdi+RUNTIME_RESUME_DISP]` / `test r11,r11` /
  `je done` — and, when one is published, the block tears its own frame down and `jmp r11` **tails**
  into it, so a return chain does not grow the native stack; otherwise the word leaves for the driver),
  plus its unit tests (the frame-discipline bytes and the emitter-side `Deallocate`+`Return` fusion).
- `heap.rs` -> `emit_allocate`, `emit_allocate_heap`, `emit_test_heap`, `emit_trim` (the heap /
  frame-admission family — a direct `emit_helper_call` to the `../runtime.rs` helpers, `allocate`
  included: one `jit_allocate` call plus the continue arm, no emitted frame push) and the private
  `emit_frame_change_continue` (the frame-only helpers' continue arm: compare against
  `encode_target(func, pc+1)`, no register refill and no emitted refresh — the helper re-published the
  Y window), plus its unit tests (the spill-first call order, the frame-only-vs-heap-admitting refill
  split, `a_frame_changing_helper_emits_no_separate_refresh_call` and the structural
  `the_allocate_arm_pushes_the_frame_with_a_helper_call_only`).
- `cons.rs` -> the `put_list` arm `emit_put_list` (one `emit_positioned_helper_call` to
  `../cons.rs::jit_put_list` — the instruction's own position published inline, since the helper stores
  none — plus a `jmp` to `fast_done`, a local `fallback` running exactly one `emit_step_and_continue`,
  and `emit_continue_check`), its refusal-only `put_list_shape` predicate, its `uses_y()` view and the
  `#[cfg(test)]` `lowered`/`declined` emission counters. Unlike the `try_inline_*` shapes it answers
  `Ok(false)` rather than `None` when it declines, so `emit_block`'s own arm falls through to
  `emit_family` in one place instead of routing on a guard predicate.
- `family.rs` -> `emit_family` (the shared helper arm for every family-owned variant: an
  `emit_helper_call` to `../family.rs::jit_family` + `emit_continue_check`) and `emit_bitstr` (the same
  arm for the `Bs*` family through `../bitstr.rs::jit_bitstr`, plus the inline position store its
  helper no longer performs — `emit_positioned_helper_call`).
- `../runtime.rs` -> `jit_step_one` / `jit_ext_call` / `jit_local_call` / `jit_tail_call` /
  `jit_deallocate` / `jit_return` / `jit_deallocate_return` / `jit_allocate` / `jit_allocate_heap` /
  `jit_test_heap` / `jit_trim` / `jit_publish_y` / `jit_refresh_y` / `JitRuntime`
  (`y_base`/`y_end`/`literals_base`/`literals_end`, `refresh_y_base`/`refresh_literals`) / the
  `encode_target`/`encode_wait`/`decode_target`/`decode_wait` codec and the
  `HALT`/`ERROR`/`ENTER`/`LOCAL_DECLINE`/`WAIT_BIT` sentinels (the helpers these emitters call).
- `../call.rs` -> `jit_call_store` + `CALL_STORED`/`CallOutcome`; `../receive.rs` -> `jit_receive`;
  `../family.rs` -> `jit_family`.

- `shapes/` → the lowering dispatch, the restricted shapes and the deopt levels; `prologue/` → the block prologue + spill/fill emission sites; `regalloc/` → the register picker and the gate derivation; `link/` → the exits, the local-call transfer and the continuation sub-blocks; `regs/` → the register cache (the only owner); `test/` → the `test` family's shapes and the refusal census. Each has its own `CONTEXT.md`.
## Notes for Agents
- Read `super::CONTEXT.md` (`../`) and the crate `CONTEXT.md` (`../../`) for the block ABI, the driver, the
  inline set and the measured figures before changing a narrowing rule.
- The invariant to preserve when adding a fast path: (a) the compile-time shape test must be *purely
  syntactic* and mirrored in `regalloc.rs::cacheable_touches`, or regalloc and `emit_block` disagree;
  (b) a run-time guard may only ever *reject* (branching to `emit_step_and_continue`), never fabricate a
  result; (c) any value left in a cache register is covered by `spill` at the next helper call / block exit.
- `baseline/tests.rs` glob-imports `emit`; `mod.rs` re-exports some items under `#[cfg(test)]` and
  `pub(crate) use block::emit_block;`/`pub(super) use list::{…};` re-exports keep every `emit::X` path
  stable across the splits. Keep that in mind when renaming items.

## Known Issues
- **`block.rs` holds the single per-instruction lowering dispatch** (`emit_block`, a `match instr` with
  one arm per instruction group) plus its deopt wrapper and the three helper-sequence arms; the `match`
  is what makes the file cohesive — each arm is one `try_inline_*` call plus an `emit_deopt` fallback —
  so splitting it further would scatter one decision across files or require inventing a visitor/table
  abstraction the emitter does not need. The block prologue it emits first is `prologue.rs`; the other
  files are near or under the ~1000-line guidance (`test.rs` 979, `arith.rs` 978, `aggregate.rs` 761,
  `move.rs` 774, `mod.rs` 490, `regs.rs` 384, `liveness.rs` 153) — `link.rs` 1136, `compile.rs` 1188,
  `regalloc.rs` 1282 and `block.rs` 1567 exceed it, each one cohesive unit (the linker, the
  compilation entry points, the register picker, the lowering dispatch); none is a single-scatter
  candidate today.
- **`test.rs` (979) and `aggregate.rs` (761) are under the guidance but still large**: `test.rs` keeps
  the plan machinery (`test_plan`/`TestPlan`/the mask constants) because both `try_inline_test` and the
  `test/header.rs` emitters read them; `aggregate.rs` keeps the tuple path while the cons-cell readers
  live in `aggregate/list.rs`.
- **A comparison `test` on two *boxed* operands is inlined only as far as its guard.** `compare_shape`
  answers `Guard::Both` for two runtime operands, so the arm loads both, guards each as a small int and
  word-compares; a rejection (either operand boxed — i.e. every structural `=:=`, `case`-clause bound
  pattern included) falls through `emit_test_helper_branch` into **one `jit_test` call** whose whole body
  is `cmp(a, b, ctx.atoms(module)).is_eq()` — a full `beam_term::compare` ordering walk, *not*
  `compare::eq` (`../test_helper.rs`). The per-comparison cost is therefore one helper call plus the
  entire structural walk.
- **That walk allocates nothing and reads no atom name for two flat maps.** `beam_term::compare::maps::cmp_sorted_flatmaps`
  borrows both keys/values slices (no pair list, no sort) after re-validating key order with one
  `cmp_flatmap_keys` per adjacent pair, and `cmp_impl`'s raw-word identity test short-circuits every
  small-int / atom / `NIL` leaf — while `cmp_flatmap_keys` compares atom **indices**, never names. So a
  decoded copy compared against its original costs zero `AtomTable::name` reads, provided both sides
  share one atom table (`beam_bifs`'s `binary_to_term` interns through a memoized image of the caller's
  shared table and commits novel names at exactly the predicted indices).
- **The canonical shape of that comparison is `copy_bound/etf_roundtrip`**, whose loop body
  `case binary_to_term(B) of T -> …` the compiler lowers to exactly this test: a bound variable in a
  pattern becomes a fresh var plus a `'=:='` guard (`lib/compiler/src/v3_core.erl:3800-3820`), which
  `beam_ssa_codegen` turns into `{test,is_eq_exact,…}` (`lib/compiler/src/beam_ssa_codegen.erl:2092-2095`).
  Nothing is planted per iteration for it — a `case` pushes no catch frame; the fail label is a static
  `case_end` block (`families/exceptions`), taken only on a mismatch.

## The bitstring shape module (`bitstr_inline/`) — emission contract
`bitstr_inline/` emits the sixteen `Bs*` lowerings — fifteen match-context shapes plus the `bs_create_bin` construction arm, whose **two shapes** (the private-append accumulator build and the bare build of `create_fresh.rs`) cover **four** routes (append, bit-granular, `binary all`, bare build) and whose helper calls all mutate no machine state; the per-shape predicates, the run-time guards and the helper-backed reasons are `../inlineset/CONTEXT.md`'s, and the file-by-file split is `bitstr_inline/CONTEXT.md`'s. This section owns only what an emitter change must keep.
- **The arms** are sixteen `match` arms in `block.rs` placed **above** the `_ if bitstrings::claimed(instr) => emit_bitstr(...)` guard arm (an arm after a `_ if` guard is unreachable), each calling its `try_inline_bs_*` entry and falling through to `emit_bitstr` when it answers `Ok(false)` having emitted **zero bytes**. The `bs_create_bin` arm is the one that never sets `terminated`: its two construction shapes continue in-block on their `fast_done` path only.
- **`emit_bit_extract(buffer, base, bit_pos, nbits, num_bytes, scratch, fallback)`** is the one field reader, and its **over-read guard is emitted before every load it protects** — `j + W <= num_bytes` in the aligned form, `j + 2 <= num_bytes` in the bit-granular one, where `j = bit_pos / 8`. Both loads are **byte-wise and assembled most-significant byte first**, because an x86 multi-byte load is little-endian and would yield the byte-reversed value with no error surfaced.
  The aligned form covers `nbits ∈ {8, 16, 32}` at `bit_pos % 8 == 0` and reads `W = nbits / 8` bytes; the granular form covers `nbits < 16` at any bit position, reading a two-byte window, shifting by `16 - rem - nbits` and masking to `nbits` bits. A `nbits > 32`, or a 16/32-bit field off an 8-bit boundary, jumps to `fallback`.
  Register contract: `base`/`bit_pos`/`num_bytes` are read-only, the sequence works in `Rcx`/`Rdi`, and `scratch` must be none of those.
- **`emit_subject_view(buffer, ctx, pos, fallback)`** answers the readable window `(base, bit_pos, num_bytes)` of the **two** subject forms it classifies, and sends every other form to `fallback` — one interpreter step, so the family answers the true value or its own typed error: non-boxed or `TAG_LITERAL_PTR` → `fallback`; a **`HEADER_SUB_BITS` inner subject whose own `orig` is not a `BinRef`** → `fallback` (a match context, whose `base_flags` is `0` and which has no payload at all); an `end < start` window → `fallback`; a masked header `!= HEAP_BITS_SUBTAG` → `fallback` (a bare `BinRef`, a tuple, an atom, … is the family's typed error). The two accepted arms:
  (a) an **on-heap `ErlHeapBits`**: `base = orig + HEAP_BITS_DATA_DISP`, `bit_pos = pos` (the form's bit offset is always 0), and `num_bytes = round_up_8(ceil(bits / 8)) = ((bits + 63) >> 6) << 3` — the block's **word-padded data extent**, not the strict `ceil(bits / 8)`, because an `ErlHeapBits` is `[header, size, data…]` with its data region padded to a whole word, so the padding bytes are allocated and readable while a field's bits always lie inside the logical subject, so the padding byte can never contribute a bit of the result. The narrow bound refused the two-byte granular window a 6-bit field of a **1-byte** subject needs and forced a per-character deopt.
  (b) a **refc window**, an `ErlSubBits` whose own `orig` is a `BinRef`: `base = (base_flags & !ERL_SUB_BITS_FLAG_MASK) + start / 8`, `rem = start % 8`, `bit_pos = pos + rem`, and `num_bytes = ceil((rem + end - start) / 8)` — the **strict** bound, never word-rounded, because an off-heap payload's reserved extent is not word-asserted and `base + num_bytes` must never exceed the bytes the window really covers. The form is expressible because the window's `start`/`end`/`base_flags` are the **inner `orig`'s own words**, which the header test reveals; the arm this tier refused earlier read the *match context's* `base_flags` (always `0`) and used absolute rather than window-relative coordinates.
  Nothing on either arm writes a machine word or allocates, so a `fallback` re-executes the instruction against the machine exactly as the interpreter left it.
  The four byte-oriented shapes — `bs_match_string` and the `bs_skip`/`bs_get_utf8`/`utf16`/`utf32` pairs — take the returned `bit_pos` for **both** their `% 8` alignment test and their `start / 8` byte index, so a refc window whose own `start` is not byte-aligned takes that shape's `fallback` instead of comparing the wrong bytes.
  `bs_get_integer2` and `bs_match`'s `integer` command also call it, handing `bit_pos` to `emit_bit_extract`, so no shape can drift from another.
- **`emit_ctx_guard`** is the match-context guard every shape opens with (`is_boxed`, then `*boxed_val == HEADER_SUB_BITS`) and it leaves the block base in `MOVE_SCRATCH` for the shape's own word reads; a shape that must also guard a `Y` destination calls `Loc::guard_store` **first**, so a rejection writes nothing.
- **The label shape is three labels.** `fallback` → `emit_step_and_continue` (exactly one interpreter step, the family's own typed error); `fast_done` → in-block continuation; `fail_arm` → `linker.emit_exit(..., ExitSpill::Required)` to the shape's resolved fail label, a **terminator on that path only**, so the hitting path still continues in-block. `emit_fallback_and_done` is the shared two-label tail; `bs_match` instead jumps to `fast_done` after its last command and reaches `fail_arm` from any command's guard.
- **Every match-context shape is position-store-free.** None of the fifteen allocates or raises, so the driver's position stays the block's; a `fallback` step stores its own through the normal deopt. The **construction** shape stores no position on its aligned route either — `jit_append_cursor` cannot pre-empt, park, raise or collect, so `bitstr_inline/create_door.rs::emit_append_cursor` emits no position store and no pre-call spill and the block entry stays published (a `fallback` taken from its `test rax,rax` arm parks through `jit_step_one`'s own arguments). The rest of the arm does publish the instruction's own `(func, pc)` before its route's door — through `emit_positioned_helper_call` for `jit_open_append_bin_any`/`_any_y` and for the `jit_write_bit_field` calls of a plan carrying a wider-than-57-bit field (one per field, narrow ones included), and for the whole-binary route's `jit_append_binary_all` — because those helpers can be entered and a `fallback` taken from their refusal arm must park on this instruction rather than on the block entry.
- **The code-size budgets.** `compile.rs::function_capacity_estimate` adds `BS_PATH_BYTES = 448` for **every** bitstring instruction and `BS_MATCH_COMMAND_BYTES = 320` for **every** accepted `bs_match` command, whether or not a predicate claims it — a predicate runs at emit time, the estimate runs before it, and an under-estimate is a `SystemLimit` compile error.
  `BS_PATH_BYTES` is sized on `bs_get_integer2`'s widest path in front of the ≈150 B `emit_step_and_continue` fallback `INSTRUCTION_BYTES` already covers, and `BS_MATCH_COMMAND_BYTES` on the widest command, `integer`; each shape that calls `emit_subject_view` pays the view's 343 B once, ~184 B of it the refc arm, and `ensure_*`/`skip` are under 60 B each.
  `CREATE_BIN_PATH_BYTES = 16_384` is added for **every** `bs_create_bin` (`create_bin_paths`), on top of that instruction's own `INSTRUCTION_BYTES`: the construction shape's stores are emitted one byte at a time, so the budget is sized on the worst accepted shape — `MAX_FIELDS = 16` fields × 8 bytes plus `MAX_TAIL_BYTES = 256` constant bytes, with the guards, the helper call and the destination store added.
  Each of the newer match-context shapes charges a path budget of its own on top of `BS_PATH_BYTES`, because its sequence is wider than the shared match-context guards it reuses: `BS_MATCH_STRING_PATH_BYTES = 2_304` per `bs_match_string` (one emitted triple per pattern byte, bounded by `match_string::MAX_MATCH_BYTES = 64`), `UTF32_PATH_BYTES`/`UTF16_PATH_BYTES` = 1 024 each and `UTF8_PATH_BYTES = 2_048`. Like the other path budgets they are charged whether or not a predicate claims the instruction, because the estimate runs before the predicate and an under-estimate is a `SystemLimit` compile error.
- **The register-cache gate** (`regalloc.rs::{cacheable_touches, cacheable_reads, cacheable_writes, is_inlined}`) and the `Y` gate (`arith.rs::uses_y`) consult the same `*_shape` functions and the label-free `<Shape>::of(...)` constructors, so the emitted register set, the block's dirty-spill set and the `Y` refresh decision are derived from one decision. A shape's destination must be charged as **written** — the value lands in that cache register, so an uncharged write lets a stale register survive a block exit.

## Store-mutation inventory (`machine.stack` / `machine.frames`)

`Machine::stack` and `Machine::frames` are private `Vec`s: no emitted code names either store, and no `JitRuntime` field publishes a header or a frame image.
A frame-changing instruction is exactly one helper call (`jit_allocate`/`jit_allocate_heap`/`jit_trim`/`jit_deallocate`), so every store mutation happens in Rust and the published Y window stays re-derivable from `Machine` state at every enterable address.
The inventory below is therefore the **precondition any future inline frame push would have to meet**, not a description of emitted traffic: such a push would have to name every site that can move `machine.stack`/`machine.frames`, prove that no emitted code reads a stale pointer after a reallocation, and keep its Y-window publication re-derivable at every enterable address rather than mid-block only.
The reason it is decidable at all is that a `Vec` write either leaves the store valid as written (any `Vec` method, the only way Rust touches the storage) or **moves the allocation** (a growth/`resize`), which invalidates any pointer taken before it.

### Erlang stack (`machine.stack`)

- `machine/frames.rs` `push_frame_window`'s `resize` — grows the store (may reallocate) and writes the Y words; re-derives the window.
- `machine/frames.rs` `pop_frame`/`pop_frame_checked`/`pop_frame_take_continuation`'s `truncate` — length only, no move.
- `machine/frames.rs` `trim`'s `copy_within` + `truncate` — in-place, no move.
- `machine/mod.rs` `with_parked_loose_terms`'s `push`/`truncate` — the dominant hidden mutator: reachable from **every** rooted hand-off, and the `push` can reallocate.
- `machine/registers.rs` `Machine::reset`'s `clear`, `machine/catch.rs` the unwind `truncate` — length only.
- In-place Y-slot word writes in `machine/frames.rs` — no reallocation.
- Root hand-offs borrow the words, never a cached pointer: `machine/mod.rs` (`RootSet::new`, `for_each_root_mut`, the census walk, `stack_mut`), `families/bitstrings/raw.rs`.

### Call frames (`machine.frames`)

- `machine/frames.rs` `push_frame_window`'s `push` — may reallocate.
- `machine/frames.rs` `pop_frame`/`pop_frame_checked`/`pop_frame_take_continuation`'s `pop` and their cold re-push.
- `machine/frames.rs` `last_mut`/`trim` field writes — in place.
- `machine/catch.rs` `truncate`, `machine/registers.rs` `clear` — length only.

### Helpers reachable from emitted code

Each of these either re-derives the Y window before the block continues, or is followed by a
driver-side (re)publication before any emitted code can use a store:

- Re-derive the window in place (they never hand out a store pointer): `jit_allocate`, `jit_allocate_heap`, `jit_test_heap`, `jit_trim`, `jit_deallocate`, `jit_deallocate_return`, `jit_tail_call`, `jit_return`, `jit_call_store`/`jit_call_store_linked`/`call_store_impl` (`call.rs`), `jit_publish_y`, `jit_refresh_y`, `refresh_y_base`, `publish_y_window`.
- Run interpreted instructions (which may push/pop/trim/`with_parked_loose_terms`) and return a block-ABI word the driver folds: `jit_step_one`, `jit_step_ext`, `jit_ext_call`, `jit_local_call`/`probe_local_call`, `jit_bif_value`, `jit_family`, `jit_receive`, `jit_bitstr`.
- Never touch the stores: `enter_in_block`/`commit_in_block_entry`/`cross_function_enter`, which only switch the entry tables.
- Driver-side: `drive` (`mod.rs`) publishes the runtime before the loop and `JitRuntime::refresh_literals` runs at every module rebind; `code.enter_cached` and the deopt arm run inside an iteration. A `traced` run never builds a `JitRuntime`.

### What an emitted store access would have to keep

- **Every capacity guard precedes every write.** A growth moves the allocation, so an emitted arm would have to re-read the store after its guards and could never name freed memory; today the whole push is the `jit_allocate` helper call, which lets the `Vec` grow and re-derives the window itself.
- **The commit order is `fill`, then `len`.** The Y words are written into spare capacity before the stack length is bumped, so a collection (or the census walk) never sees a length covering a word the push has not written.
- **The Y-window publication must be re-derivable at every enterable address.** The window follows from `Machine` state alone (`push_frame_window`'s `(y_base, y_count)` on a push, `refresh_y_base` on a `trim`/pop), so a block entry or a resumed slice can always re-derive it: the in-block `allocate`/`allocate_heap`/`trim` helper re-publishes it on the arm that continues the block, a block entry re-executes the block prefix, and the prologue publishes whenever the block's first Y access is not preceded by such a helper. `../frame_push_tests.rs` pins the behavioural half (each frame pushed exactly once across a sliced and resumed run) and `heap.rs`'s `the_allocate_arm_pushes_the_frame_with_a_helper_call_only` the structural half.
