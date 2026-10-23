# `emit/prologue` — the compiled-block prologue and the spill/fill discipline

## Intent
What every emitted range opens with (`prologue.rs::emit_block_prologue`: the variable-length frame, the `rt` slot, the position store / Y-window publish, the live-in cache load) and the spill/fill traffic every later site follows.
The cache itself (`Allocation`/`spill`/`fill`/`SpillSite`/`FillSite` and its measured models) is owned by `../regs/CONTEXT.md`; this node owns the emission sites.
`rsp` stays 16-byte aligned at every helper call (`frame_slot_len(k)` = `8` when `k = 1 + slots.len()` is even, `16` when odd), and the teardown (`../CONTEXT.md`'s `emit_frame_teardown`) derives the matching `lea rsp,[rsp+frame_slot_len(k)]` from the same `native_frame_regs`.

## Spill/fill discipline
- Prologue (`prologue.rs::emit_block_prologue`, emitted first by `block.rs::emit_block`): push the
  block's own frame register set
  (`native_frame_regs` — `rbx` plus the machine registers the block holds as cache slots),
  reserve the `frame_slot_len(k)`-byte `rt` slot (`8` when `k = 1 + slots.len()` is even,
  `16` when odd, so `rsp` stays 16-byte aligned at every helper call), `rbx = rt->x_regs`,
  then call `jit_set_position(rt, func, block_start_pc)` (the block-entry
  position store — emitted before the cached loads, because `rbx` is callee-saved and no cached register
  may sit in caller-saved scratch across the call) — or, when the block must publish the window
  (`Allocation::prologue_publishes_y`: it accesses a Y slot and **not** every one of those accesses
  follows an in-block `allocate`/`allocate_heap`/`trim`, whose helper re-published the window itself),
  `jit_publish_y(rt, func, pc)`, which does that same position store **and** re-publishes the current
  frame's Y window in one call — then load each cached register in
  `Allocation::entry_fill` from `machine.x` (the block's **live-in** slots: a cached register the block
  writes before it reads is not loaded at entry, because that writing instruction sets it).
  The frame is **variable-length**: `k = native_frame_regs(allocation).len()` is the block's own push count,
  so the reservation and the teardown's matching `lea rsp,[rsp+frame_slot_len(k)]` always agree
  (`emit/link.rs::emit_frame_teardown` derives both sides from the same `native_frame_regs`). Corpus
  counters (grand total over **that recorded pass's** corpus — the then-32 committed `erlc` fixtures +
  a 1321-module installed-OTP sample; the corpus' current size is `../../census/CONTEXT.md`'s):
  597 333 blocks / 756 632 cache slots, push/pop ops 7 167 996 → 2 707 930 (−62.2 %), emitted bytes
  320 065 790 → 306 024 443 (−4.39 %), with every per-block cache/write set and IR size unchanged.
  The prologue load set is `Allocation::entry_fill`, which cuts corpus prologue loads
  **756 632 → 352 537** (−53.4 %).
- `regs.rs::spill` / `regs.rs::fill`: `spill(buffer, allocation, site, pc)` stores exactly
  `Allocation::spill_slots_at(pc)` — the cached x-registers **dirty at that pc** (`Allocation::spill_sets`,
  the `regalloc.rs` forward dirty walk) — falling back to the whole `Allocation::written` set when the
  table is empty (a synthetic `Allocation` a unit test built directly); `fill` loads every `(x, reg)` in
  `Allocation::fill_slots(pc)` — the continuation's block-local liveness unioned with the write set —
  except the prologue load, which is `Allocation::entry_fill` (the block's live-in cached slots; see the
  **Register cache** section below).
- **Every helper call spills first.** `mod.rs::emit_helper_call(buffer, allocation, helper, args, pc)`
  (used by `emit_step_and_continue`, `emit_ext_call`, `emit_receive`, `emit_family`, the `Call` arm, the
  frame-discipline trio, the heap/frame-admission family and the comparison-`test` helper) stores the
  dirty set at that instruction's own `pc` before loading `rdi` from
  the `rt` slot, filling the ABI argument registers `rsi`/`rdx`/`rcx`/`r8`/`r9` with the caller's
  argument words, and `call r11`; it does **not** spill after the call (the helper legitimately wrote
  `machine.x`) — `fill` reloads instead. `link.rs::emit_tail_exit` spills before `call jit_tail_call`.
  No emitted path in this node calls a helper without spilling first.
- **Every spill site passes the instruction's own `pc`.** `emit_helper_call` stores at the instruction's
  `pc`; `link.rs::emit_exit`/`emit_tail_exit`/`emit_tail_exit_to_driver` take a trailing `store_pc`
  **separate** from their branch target `(func, pc)` (a `jump`/`call`/`call_last`/`call_only` passes its
  own `pc` as `store_pc` and the successor `(func, pc)` as the target; the block's fall-through exit
  passes the exit pc `end`); the fused `deallocate N; return` pair stores at the pair's **first** `pc`
  (`emit_deallocate_return`'s `store_pc`) while the helper's recorded position is the `return`'s pc
  (`position_pc`).
- **Gated Y-window refresh after an in-block helper — only for a helper that changed no frame.**
  `emit_continue_check` (the shared continue arm of `emit_step_and_continue`, `emit_ext_call`,
  `emit_receive`, `emit_family` and `test_heap`) emits `jit_refresh_y(rt)` — after `fill`, on the
  continue-in-block path only — exactly when a later instruction of the block still accesses a Y slot
  (`Allocation::refresh_y_after(pc)`), because the helper ran one instruction in the interpreter, which may
  have **reallocated** `machine.stack` and so staled the prologue's window. When no later instruction
  accesses Y the call is not emitted at all, and a helper that leaves the block to the driver pays nothing
  (the next block's prologue publishes).
  The **frame-changing** helpers (`jit_allocate`, `jit_allocate_heap`, `jit_trim`, `jit_deallocate`) do not
  use this arm: they are the parties that invalidate the window (`push_frame`/`trim`/`pop_frame*` move
  `frames.last()` and truncate `machine.stack`) and they re-publish it **inside** the helper on their success
  path, via `JitRuntime::refresh_y_base` — a pure re-derivation from machine state. `emit/heap.rs::
  emit_frame_change_continue` (frame-only: `allocate`/`trim`, no register refill) takes no `Allocation` for
  that reason, and `emit/mod.rs::emit_continue_check_helper_refresh` is `emit_continue_check` **minus** the
  refresh call but **keeping** the partial refill, used by `allocate_heap` because a collecting host's
  admission rewrites `machine.x` in place. `test_heap` keeps the gated refresh (it changes no frame).
  Removing the emitted call is sound because the emitted Y guard is **reject-only** (`jae` →
  `jit_step_one`) and `y_base == y_end == null` rejects every slot, so the worst reachable state is
  "not yet published", never a stale window.

## See Also
- `../../CONTEXT.md` → the driver, the block ABI and the position store's publication set.
- `../regs/CONTEXT.md` → the register cache the spill/fill sets come from.
- `../regalloc/CONTEXT.md` → `entry_fill`/`spill_sets`/`refresh_y_after` derivation.
- `../link/CONTEXT.md` → the exits that carry `ExitSpill` and the `Done` single-store rule.
