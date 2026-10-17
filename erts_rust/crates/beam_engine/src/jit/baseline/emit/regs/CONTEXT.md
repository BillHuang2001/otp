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
- **Precondition: an `Allocation` is used only for the range it was built for, and emission starts at its
  own `block_start`.** The static `spill_sets`/`fill_sets` tables are indexed by `pc - block_start`
  (`spill_slots_at`/`fill_slots`), and a table that does not cover `pc` conservatively means the whole
  `written` set (spill) / the whole cache (fill) -- sound, but not the allocation's real traffic. The `pc`
  every emitter passes is the `store_pc`, i.e. the pc **entering** the instruction being lowered (the
  instruction's own address, not its successor's or its branch target's). The precondition is enforced
  structurally: registers are allocated per emitted sub-range by
  `allocate_registers(module, code, start, end)` (`../compile.rs`, for the IR blocks **and** the
  continuation sub-blocks), so a sub-range never inherits another range's pc-indexed tables.
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

## Notes for Agents (measured and rejected — do not re-investigate)
- **Further fill-side refinement is NOT available (measured, not assumed).** `fill` already loads only `(live ∪ written) ∩ slots`, and the measured headroom between it and the whole-cache fill is small: the corpus census prints full-cache **3 950 215** fill loads vs liveness **3 695 119** (**255 096**, ~6.5 %), and the two-process probe prints full **75** vs partial **69**.
- **No sound per-helper "clobber set" exists to close that gap.** A heap-admitting helper (the `test_heap`/`allocate_heap`/`allocate` route into `ExecutionContext::ensure_heap_full`/`alloc_words_rooted`) may be a **collecting** host that rewrites every published root in `machine.x`, so it must refill the whole cache; the value helpers (`jit_bif_value`, `jit_test`) can allocate through the engine's own `read_operand`; and the frame-only helpers (`jit_deallocate`/`jit_return`/`jit_tail_call`) already emit no fill at all, so they are not the headroom.
- **The `CACHE_REGISTERS` criterion is pinned and has no counter to rank on.** Membership is the live-interval-descending selection of `../regalloc.rs::allocate_registers` (ties by ascending register index), pinned by `../tests.rs`/the crate's tier tests; the harness records no per-register hit or benefit counter, and a register's live interval already dominates its touch count.
- **The shipped dirty-set model, measured on the corpus (the numbers, so they need not be re-derived).** `census.rs::the_register_cache_traffic_census_over_the_corpus_is_printed` (32 committed `erlc` fixtures + a 1321-module installed-OTP sample = 103 008 functions / 597 333 blocks / 3 585 490 IR instructions) prints **all three** store models from one pass, so the before/after is a single measurement: whole-cache **5 578 219**, written-set **4 678 655**, **dirty-set 1 594 678** (−65.9 %; 3 083 977 redundant already-clean stores removed), with emitted bytes **341 653 629 → 320 065 790** — exactly 7 B per removed store. Fixtures only: 342 dirty / 1 035 written / 1 473 full-cache stores, `code_len` **253 272 → 248 288 B**. The two-process probe (`../tests.rs::the_two_process_copy_bound_shape_is_measured_on_both_tiers`) reads spill stores **82 → 18** (`Helper` 58→14, `Exit` 24→4) against a 102 whole-cache model, emitted bytes 8 266 → 7 818, fill loads 69 (unchanged).
- **The batch's wall-clock effect is NOT attributable.** `jit_bench`'s 13 tier ratios (baseline-jit ÷ interpreter) span **0.12×–1.22×** at `/proc/loadavg` 42–57 on 14 cores, and two run-sets minutes apart disagree on the same binary (`wide/2` 0.134× then 0.126×, `cross/2` 0.952× then 0.862×) — a spread that exceeds any effect of removing stores from a slice. Structural counters are the evidence here; do not re-open the wall-clock question without a quieter host.

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
by construction). The **corpus-wide** evidence is
`../census.rs::the_register_cache_traffic_census_over_the_corpus_is_printed`, which prints one run's
three spill **models** (the dirty set `spill` emits, the whole-block written set, the full cache),
both fill models, the calls/blocks by site and size, and the **redundant-clean-store difference**
(`written − dirty`, the exact size of the store-elimination opportunity) for the committed fixtures
and the installed-OTP corpus; like the other censuses it asserts liveness/consistency only, never a
threshold.

## See Also
- `CONTEXT.md` (this directory) -> the spill/fill discipline in `emit_block` and the call sites.
- `../CONTEXT.md` -> the baseline tier (artifact layouts, block ABI, driver).
- `../../counters.rs` -> the test-only structural counters (the recorded spill/fill counts from one run).
