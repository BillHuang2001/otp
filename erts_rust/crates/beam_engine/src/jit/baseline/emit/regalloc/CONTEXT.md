# `emit/regalloc` — the register picker and the block's gate derivation

## Intent
`regalloc.rs::allocate_registers`: the live-interval x-register selection (≤5 cache slots) and the three gates derived from the same scans — the Y publish/refresh triple (`y_last`/`y_first`/`frame_republish_first` → `prologue_publishes_y`) and the per-`pc` dirty set (`spill_sets`).
The selector is a greedy linear scan by descending interval length, tie-broken by ascending register index; physical assignment is by first-appearance order, so it is deterministic.
Membership is **fixed for the whole block** — the correctness contract that keeps the two representations from disagreeing.

## Register picker (`regalloc.rs::allocate_registers`)
Liveness-based, by **live interval**. `cacheable_touches` reports the x-register operands **of each
instruction the tier would actually inline** (it mirrors each lowering's own shape test, so regalloc and
`emit_block` agree); `cacheable_writes` reports the same instructions' destination registers. A candidate
register's live interval is `[first touch, last touch]` over the block's instruction indices, its touches
being the union of the two sets (so a register an instruction only *writes* is live there too).
The `[first, last]` span is deliberately the *same* predicate `emit/liveness.rs::fill_sets` derives its
live set from, so the selector and the emitter share one definition of liveness.
Selection is a greedy linear scan over those intervals: **primary key** = interval length `last - first + 1`,
**descending**; **tie-break** = **ascending register index**; the top `CACHE_REGISTERS.len()` = 5 are kept
(`r12`,`r13`,`r14`,`r15`,`rbp`). Physical assignment is by **first-appearance order** of the selected
registers (deterministic), not by rank. An out-of-bound register (`>= MAX_REGISTER`) is never a candidate.
Every non-selected register is read/written directly in `machine.x`, which is exactly the traffic an in-block
eviction would produce — but membership stays **fixed for the whole block** (the correctness contract).
Under `#[cfg(test)]` the scan also records live-range statistics on `Allocation`
(`live_candidates` / `live_total` / `live_max`), which `counters::record_block` folds into the per-run
structural counters; they hold no production state.

`allocate_registers` also derives the block's Y-gate triple, all from the same `arith::uses_y` scan (the
union of every lowering's per-shape Y test — including `bif::bif_uses_y` for the named-BIF accessors,
whose `Value` route is never a Y-window access): `Allocation::y_last` (the highest pc whose instruction
accesses a Y slot — the compile-time gate for the emitter's post-helper refresh), `Allocation::y_first`
(its twin, the lowest such pc) and `Allocation::frame_republish_first` (the lowest pc of an
`allocate`/`allocate_heap`/`trim`, whose helper calls `JitRuntime::refresh_y_base` on the arm that
continues the block in-block). `y_first` and `frame_republish_first` together answer
`Allocation::prologue_publishes_y`, the prologue-publish gate: a block publishes **unless** every Y
access lies at or after that helper, because then the helper's own re-publish overwrites the prologue's
window before anything reads it (a block's control flow is linear — it is entered only at its own start,
and every way out of the middle lands in another emitted range with its own prologue). A `Y` slot is
**not** an x-register, so it is neither a candidate nor charged to any live interval, and it never raises
`highest_native_register`: a stack slot must not refuse native entry (the driver's bound is
`machine.x.len() > bound`).

`allocate_registers` also derives `Allocation::spill_sets` — the block's per-`pc` **dirty** set, the store
set every `spill` uses. It is a forward walk over the block: push the dirty set entering each instruction,
union `cacheable_writes` when the instruction inlines (`is_inlined`, which mirrors the emitter's own
compile-time inlining decision and is biased toward "inlined"), and clear it otherwise (a purely-helper
instruction's own pre-call spill synced the whole cache, so nothing is dirty after it). The final entry
is the dirty set at the block exit pc `end`.

## See Also
- `../../CONTEXT.md` → the block ABI, the register guard the derived bound feeds.
- `../regs/CONTEXT.md` → the cache this picker fills; the measured models.
- `../prologue/CONTEXT.md` → where the prologue gate and the refresh gate are emitted.
- `../shapes/CONTEXT.md` → the per-lowering shape tests `cacheable_touches`/`cacheable_writes` mirror.
