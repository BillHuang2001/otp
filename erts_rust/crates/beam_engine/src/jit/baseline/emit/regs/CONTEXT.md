# `regs` -- Baseline JIT block register cache

## Intent
The **register cache** of the native baseline x86-64 tier: which x-registers a compiled block holds in
machine registers for the whole block, and the two directions of that cache's contract (`spill` back to
`machine.x`, `fill` out of it). Extracted from `mod.rs` so the cache's contract, its soundness argument
and its test-only site classification live in one ~300-line file. Owned by `./erts_rust/crates/beam_engine/src/jit/baseline`.

## API Surface (crate-visible)
- `Allocation` -- a block's register allocation.
  - `slots: Vec<(u32, Reg)>` -- the cached `(x-register index, machine register)` pairs, in assignment order.
  - `written: Vec<u32>` -- the cached x-register indices some inline lowering of the block **may write**
    (a subset of `slots`, in slot order). It is the whole-block conservative **fallback** `spill` stores
    when `spill_sets` is empty. See the soundness contract below.
  - `spill_sets: Vec<Vec<u32>>` -- the **dirty set of the cache at every pc** of the block, indexed by
    `pc - block_start`: the cached x-registers a `spill` at that pc must store. Always one entry per pc in
    `[block_start, end]` -- **including the block exit pc `end`** -- derived by `../regalloc.rs`'s forward
    dirty walk. An **empty** table means "no dirty analysis was computed".
  - `y_last: Option<u32>` -- the highest pc of the block whose instruction accesses a `Y` slot (the
    compile-time gate for the prologue Y-window publish and the gated post-helper refresh).
  - `slot(x)`, `is_written(x)`, `spill_slots()` (the whole-block written `(x, Reg)` pairs),
    `spill_slots_at(pc)` (the `(x, Reg)` pairs **dirty at `pc`**, else `spill_slots()` when the table is
    empty or does not cover `pc`), `refresh_y_after(pc)`.
- `spill(buffer, allocation, site, pc)` -- stores `allocation.spill_slots_at(pc)` (the cached registers
  **dirty at the instruction's own `pc`**) via `mov [rbx + x_offset(x)], reg`. `rbx` holds the `machine.x`
  base. `site` classifies the call for `counters.rs` only; the recorded count is the number of stores
  actually emitted.
- `fill(buffer, allocation, site, pc)` -- loads exactly `allocation.fill_slots(pc)` via `mov reg, [rbx + x_offset(x)]` (the whole cache at `FillSite::Prologue`, the continuation's live set elsewhere).
- `SpillSite { Helper, Exit }` / `FillSite { Helper, Prologue }` -- test-only classification of a call
  site for `counters.rs`; the parameter is unused outside `#[cfg(test)]` (zero cost in production).
  `FillSite::Prologue` is `#[cfg(test)]`-gated (only the prologue counter names it; the production
  prologue loads the cache inline).

## The soundness contract (dirty-set spill at each pc, partial fill, fixed membership)
- **Membership is fixed for the whole block.** An x-register is either *always* held in its machine
  register -- and written back before the block leaves its frame -- or *never* cached, so the two
  representations can never disagree.
- **`spill` stores only the registers dirty at its own `pc`.** A cached register is *dirty* when an
  inline lowering has diverged its machine register from its `machine.x` slot since the last syncing
  event -- the prologue (which loads the cache from `machine.x`) or a post-helper `fill` (which reloads
  it). A clean register holds exactly its `machine.x` value, so storing it writes back an unchanged word.
- **The dirty set is a static per-`pc` over-approximation**, derived by `../regalloc.rs`'s forward
  `spill_sets` walk: it unions `cacheable_writes` for an inlined instruction and resets where the
  emission is entirely a helper (whose own pre-call `spill` synced the whole cache), biased toward
  "inlined" so a missed reset only costs a redundant store while a wrong reset would drop a dirty value.
- **`written` stays the conservative fallback**: an allocation whose `spill_sets` table is empty or does
  not cover the `pc` (a synthetic one a unit test built directly) spills the whole-block write set.
- **`fill` is partial**: it reloads only the cached registers the continuation at `pc + 1` can still
  observe -- the block-local liveness of `../liveness.rs::fill_sets` unioned with the write set,
  `(live ∪ written) ∩ slots` -- except the prologue load, which is the whole cache. A register left out of
  the set keeps a stale word that is only ever overwritten, never read.

## Design Decisions
- **Already-clean-aware ("dirty-set") stores are the classic handwritten-JIT register-cache store elimination.** The model and its soundness argument follow `ERTS_RUST_REWRITE_DEEPDIVE.md` §F (the baseline/hot tier split and its correctness-first register-cache design) and the read-only legacy-C reference `erts/emulator/beam/jit/x86/beam_asm*.cpp` with `erts/emulator/beam/jit/beam_jit_register_cache.hpp`; note that reference is a *position-keyed whole-cache* cache (`consolidate(offset)`/`validAt(offset)` evict the whole cache when the emit position moves, with **no** per-register dirty bit), so the dirty-set rule is this tier's own, not a port of that file. The full design note (the walk, the reset rule, the citation) is in `../regalloc.rs`'s module docs.
- **The dirty set is computed statically at emission time, never tracked at run time** — zero cost in emitted code: `spill_slots_at(pc)` is a compile-time table lookup, so a run-time block is the same shape as under the written-set model apart from how many stores it emits.

## Constraints
- The crate is `#![deny(unsafe_code)]`; this file dereferences no raw pointer (it only emits bytes).
- `spill`/`fill` return `beam_common::Result<()>`; a `CodeBuffer` overflow is the typed error, never a panic.
- Files ~1000 lines; Apache-2.0 header byte-exact.

## Test Strategy
`../spill_tests.rs` (the `#[cfg(test)] #[path]` submodule `spill_tests` of `mod.rs`) pins the dirty-set
store set: a write followed by a helper stores the register once, an inlined instruction that writes
nothing does not reset the set, a fully-deopted one does reset it, an empty `spill_sets` table falls back
to the whole `written` set, and a corpus consistency pin checks `../regalloc.rs::is_inlined` against the
emitter's own inlining decision. `../counters.rs` unit test
`one_run_yields_both_the_written_and_the_full_cache_spill_counts` pins that a
4-slot cache with a 2-element write set at two spill sites yields **4** written-set stores and **8**
full-cache stores from one recorded run. The emitted bytes themselves are pinned by
`baseline/tests.rs` (`every_exit_spills_the_cached_registers_before_leaving_the_frame`,
`a_tail_exit_spills_pops_the_frame_and_then_jumps_to_the_successor`,
`a_deallocate_spills_before_calling_the_helper`), which assert against the **dirty-set** store
count. Tier-equivalence is proven by the crate's `tests/jit*.rs` real-module gates (interpreter-identical
by construction).

## See Also
- `CONTEXT.md` (this directory) -> the spill/fill discipline in `emit_block` and the call sites.
- `../CONTEXT.md` -> the baseline tier (artifact layouts, block ABI, driver).
- `../../counters.rs` -> the test-only structural counters (the recorded spill/fill counts from one run).
