# `regs` -- Baseline JIT block register cache

## Intent
The **register cache** of the native baseline x86-64 tier: which x-registers a compiled block holds in
machine registers for the whole block, and the two directions of that cache's contract (`spill` back to
`machine.x`, `fill` out of it). Extracted from `mod.rs` so the cache's contract, its soundness argument
and its test-only site classification live in one ~180-line file. Owned by `./erts_rust/crates/beam_engine/src/jit/baseline`.

## API Surface (crate-visible)
- `Allocation` -- a block's register allocation.
  - `slots: Vec<(u32, Reg)>` -- the cached `(x-register index, machine register)` pairs, in assignment order.
  - `written: Vec<u32>` -- the cached x-register indices some inline lowering of the block **may write**
    (a subset of `slots`, in slot order). See the soundness contract below.
  - `y_last: Option<u32>` -- the highest pc of the block whose instruction accesses a `Y` slot (the
    compile-time gate for the prologue Y-window publish and the gated post-helper refresh).
  - `slot(x)`, `is_written(x)`, `spill_slots()` (only the written `(x, Reg)` pairs), `refresh_y_after(pc)`.
- `spill(buffer, allocation, site)` -- stores **only** `allocation.spill_slots()` (the written set) via
  `mov [rbx + x_offset(x)], reg`. `rbx` holds the `machine.x` base.
- `fill(buffer, allocation, site, pc)` -- loads exactly `allocation.fill_slots(pc)` via `mov reg, [rbx + x_offset(x)]` (the whole cache at `FillSite::Prologue`, the continuation's live set elsewhere).
- `SpillSite { Helper, Exit }` / `FillSite { Helper, Prologue }` -- test-only classification of a call
  site for `counters.rs`; the parameter is unused outside `#[cfg(test)]` (zero cost in production).
  `FillSite::Prologue` is `#[cfg(test)]`-gated (only the prologue counter names it; the production
  prologue loads the cache inline).

## The soundness contract (written-set spill, full fill, fixed membership)
- **Membership is fixed for the whole block.** An x-register is either *always* held in its machine
  register -- and written back before the block leaves its frame -- or *never* cached, so the two
  representations can never disagree.
- **`spill` stores only `written`.** A cached register outside `written` holds exactly its `machine.x`
  value at every program point of the block: the prologue loads it, no inline lowering diverges the two,
  and every post-helper `fill` re-syncs it (the helper *is* the interpreter and may have written
  `machine.x`). Storing such a register writes back the value it already holds, so skipping it is a
  no-op.
- **The write set is a static over-approximation**, derived by `regalloc.rs::cacheable_writes` mirroring
  each lowering's own shape test (a register the block writes but the set omitted would let a stale cache
  register survive an exit). An over-approximation costs a redundant store but can never omit one.
- **`fill` stays full**: the helper it follows may have written *any* x-register, so every cached
  register is reloaded -- no per-register liveness is needed.

## Constraints
- The crate is `#![deny(unsafe_code)]`; this file dereferences no raw pointer (it only emits bytes).
- `spill`/`fill` return `beam_common::Result<()>`; a `CodeBuffer` overflow is the typed error, never a panic.
- Files ~1000 lines; Apache-2.0 header byte-exact.

## Test Strategy
`counters.rs` unit test `one_run_yields_both_the_written_and_the_full_cache_spill_counts` pins that a
4-slot cache with a 2-element write set at two spill sites yields **4** written-set stores and **8**
full-cache stores from one recorded run. The emitted bytes themselves are pinned by
`baseline/tests.rs` (`every_exit_spills_the_cached_registers_before_leaving_the_frame`,
`a_tail_exit_spills_pops_the_frame_and_then_jumps_to_the_successor`,
`a_deallocate_spills_before_calling_the_helper`), which assert against the **pruned** written-set store
count. Tier-equivalence is proven by the crate's `tests/jit*.rs` real-module gates (interpreter-identical
by construction).

## See Also
- `CONTEXT.md` (this directory) -> the spill/fill discipline in `emit_block` and the call sites.
- `../CONTEXT.md` -> the baseline tier (artifact layouts, block ABI, driver).
- `../../counters.rs` -> the test-only structural counters (both spill models from one run).
