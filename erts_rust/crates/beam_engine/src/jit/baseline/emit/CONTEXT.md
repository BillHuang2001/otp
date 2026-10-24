# `baseline/emit` — the compiled-block emitter

## Intent
Lower one `beam_loader` IR block into a real `extern "C"` x86-64 function: the prologue (variable-length
frame, position store / Y-window publish, live-in cache fill), one emitted sequence per instruction, and
the shared exits/continuation check. Every instruction the per-instruction `match` does not lower falls to
`emit_deopt` (exactly one interpreter step), so a tier is interpreter-identical for every supported opcode.

## API Surface
- `compile.rs` — `compile_module` (eager, one sealed buffer), `compile_function` (lazy, per function),
  `emit_function`, the continuation sub-blocks (`stores_continuation`, `sub_blocks_capacity_estimate`,
  `emit_sub_blocks`, `emit_shared_sub_blocks`) and the code-size estimates (`code_capacity_estimate`,
  `function_capacity_estimate`, `BLOCK_BYTES`, `INSTRUCTION_BYTES`, `FAST_PATH_BYTES`,
  `COMPARE_PATH_BYTES`, `LIST_PATH_BYTES`, `TUPLE_PATH_BYTES`). The buffer never grows; an
  under-estimate is a `SystemLimit` compile error, so every new emission must be priced.
- `block.rs` — `emit_block`, the per-instruction dispatch (`match instr`), `emit_deopt` (static-census
  record + `emit_step_and_continue`), `emit_step_and_continue`, `emit_in_block_call`,
  `emit_local_call_probe`.
- `mod.rs` — shared wiring: `SAVED_REGISTERS`/`CACHE_REGISTERS`/`RUNTIME_SLOT_DISP`/`MOVE_SCRATCH`,
  `emit_helper_call`, `emit_positioned_helper_call`, `emit_continue_check`,
  `emit_continue_check_helper_refresh`, `emit_refresh_y`, the submodule list, and the `#[cfg(test)]`
  re-exports `baseline/tests.rs` glob-imports.
- Per-group emitters: `r#move.rs`, `test.rs` (+ `test/header.rs`, `test/tests.rs`,
  `test/refusal_census.rs`), `select.rs`, `arith.rs`, `compare.rs`, `aggregate.rs` (+ `aggregate/list.rs`),
  `frame.rs`, `heap.rs`, `family.rs`, `bitstr.rs` (the native `Bs*` shapes), `link.rs`, `operands.rs`,
  `liveness.rs`, `regalloc.rs`, `regs.rs`, `prologue.rs`, `shapes/`.

## Constraints
- **A shape predicate is purely syntactic** (operand kinds, resolved import names, constant operands) and
  is mirrored in `regalloc.rs::cacheable_touches`/`cacheable_reads`/`cacheable_writes` and `is_inlined`;
  a register the block writes but `cacheable_writes` omits is the one unsound failure mode.
- **A run-time guard may only ever reject** (branch to a local `fallback` that runs one interpreter step,
  or to `emit_step_and_continue`/`Linker::emit_exit`); it may never fabricate a value or leave a
  destination written.
- **Any value left in a cache register must be covered by `spill` at the next helper call or block exit.**
- A native shape that neither allocates nor raises needs no instruction position store (the block's entry
  position stands; a deopt re-runs the interpreter step, which publishes its own).
- Reserve → read → write: no inline arm may hold an `Eterm` across an allocating call.
- `#![allow(unsafe_code)]`, every `unsafe` block carries `// SAFETY:`; files ~1000 lines.

## The bitstring shapes (`bitstr.rs`)
Native, guarded, straight-line lowerings for the matching family's static shapes — the shape set, each
run-time guard and each fallback are recorded in `../inlineset/CONTEXT.md`; the emitted-byte arithmetic
and the frames live in this file's per-shape doc comments.

## Notes for Agents
- `emit/block.rs`'s dispatch is one ordered `match`; the bitstring family's `_ if
  bitstrings::claimed(instr)` **guard arm** and the family list follow the specific arms, so a new
  variant-specific arm must be inserted **above** the guard arm.
- `emit_mem_modrm` always emits `mod=10 disp32`, so every sequence's size is a closed form — price a new
  emission before assuming `INSTRUCTION_BYTES` covers it.
- The x86 encoder (`../../emitter/encode.rs`) has no logical right shift and no narrow loads: a shape
  needing `shr` or a byte/word/dword load must use the encoder's provided forms (`shr_ri`, `shl_ri`,
  `movzx_byte`, `movzx_word`, `mov_rm32`, `shr_cl`, `shl_cl`).

## Routing Table
- `shapes/` → the shape predicates and the per-shape lowering rules
- `test/` → the comparison-`test` lowering, its header-kind subset and the refusal census
- `aggregate/` → the boxed-term readers (cons/tuple) and the tuple writer
- `prologue/` → the block prologue, the position store and the Y-window publish
- `regalloc/` → the register picker and the live-interval analysis
- `regs/` → the `Allocation` and the register-cache model
- `link/` → the in-block exits and the `Linker`
- `spill/` → the spill/fill call-site discipline
