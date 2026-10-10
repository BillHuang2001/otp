# `emit` -- Baseline JIT x86-64 Instruction Emitters

## Intent
Lower the typed IR basic blocks of a `beam_loader::Module` into native x86-64 functions for the
**baseline** execution tier. One file per responsibility: `mod.rs` owns the compiled-block shape
(prologue/epilogue, `emit_block`'s lowering dispatch, the deopt sequence, `spill`/`fill`,
`compile_module`); `regalloc.rs` picks the cached x-registers; `operands.rs` reads operand shapes;
`r#move.rs`/`test.rs`/`select.rs`/`arith.rs`/`compare.rs`/`aggregate.rs` each lower one instruction
group; `link.rs` owns the direct block-to-block jump and the linked local tail call.
The driver, block ABI and the three helpers (`jit_step_one`, `jit_ext_call`, `jit_tail_call`) live one
level up in `../mod.rs` / `../runtime.rs`; this node only *emits calls* to them.

## Core model (do not break)
- **Only `CACHE_REGISTERS.len()` = 5 x-registers are held in machine registers** (`r12`-`r15` + `rbp`,
  `mod.rs` `CACHE_REGISTERS`); every other x-register an instruction names is addressed **directly in
  `machine.x`** via `[rbx + x*8]` (`rbx` = `rt->x_regs`, loaded in the prologue). A register is either
  *always* cached in its machine register or *never* cached, fixed for the whole block, so the two
  representations can never disagree.
- **Spill is the whole liveness/root-set model.** `spill` writes every cached register back to
  `machine.x`; `fill` reloads all of them after a helper call (no per-register liveness). Both are in
  `mod.rs`.
- Caller-saved scratch registers, used only *within* one instruction's lowering: `MOVE_SCRATCH`=`r11`,
  `MOVE_SCRATCH_2`=`r10` (`mod.rs`); `FAST_A`=`rax`, `FAST_B`=`r8`, `FAST_C`=`rcx`, `FAST_T`=`r10`,
  `FAST_R`=`r11` (`arith.rs`); `TEST_A`=`rax`, `TEST_B`=`r8`, `TEST_C`=`r9`, `TEST_T`=`r10`,
  `TEST_H`=`r11` (`test.rs`);
  `GUARD_SCRATCH`=`r8` (`aggregate.rs`). None overlaps a cache register, `rbx` or `rsp`.

## The lowering dispatch (the compile-time inlinable-vs-deopt decision)
`mod.rs::emit_block` matches each `Instr` of the block (the `match instr { .. }`). Each arm calls the
group's `try_inline_*`; **`Ok(false)` means "lowered nothing"** and the arm then emits
`emit_step_and_continue`. Whole-instruction inlining therefore is a **compile-time, syntactic** decision
(never a value test). Logical groups:

| Group | Instr(s) | Lowering entry | Shape/guard predicate |
|---|---|---|---|
| Metadata no-ops | `Label`,`Line`,`IntCodeEnd`,`ExecutableLine`,`DebugLine` | `mod.rs` empty arm (no bytes) | n/a |
| Move / swap | `Move`, `Swap` | `r#move.rs::try_inline_move`, `::try_inline_swap` | dst must be in-bound `X(n)` |
| Test (word + boxed header) | `Test` | `test.rs::try_inline_test` | `test.rs::test_plan` |
| Switch | `SelectVal` | `select.rs::try_inline_select_val` | `select.rs::select_val_plan` |
| Arity switch | `SelectTupleArity` | `aggregate.rs::try_inline_select_tuple_arity` | `aggregate.rs::select_tuple_arity_plan` |
| Cons readers | `GetList`,`GetHd`,`GetTl` | `aggregate.rs::try_inline_get_{list,hd,tl}` via `emit_list_read` | `get_{list,hd,tl}_shape` -> `shape` |
| Tuple read | `GetTupleElement` | `aggregate.rs::try_inline_get_tuple_element` via `emit_tuple_read` | `get_tuple_element_shape` |
| Arithmetic gc_bif | `GcBif` (`+`/`-`/`*`) | `arith.rs::try_inline_gc_bif` | `arith.rs::fast_arith` |
| Comparisons | `Bif` (and `GcBif` fallback) | `compare.rs::try_inline_comparison` | `compare.rs::comparison_shape` |
| Jump | `Jump` | `link.rs::Linker::emit_exit` | resolved label -> linked or return-to-driver |
| Local tail call | `CallLast` | `link.rs::Linker::emit_tail_exit` | `Linker::is_compiled` |
| Local tail call (no pop) | `CallOnly` | `link.rs::Linker::emit_exit` (lowers like `Jump`) | resolved label |
| Cross-module call | `CallExt`,`CallExtOnly`,`CallExtLast` | `mod.rs::emit_ext_call` (an `emit_helper_call` to `jit_ext_call`) | n/a — the helper resolves the import and reports `ENTER` |
| Everything else | -- | `_` arm -> `emit_step_and_continue` | -- (`FuncInfo` is deliberately here, never a no-op) |

Note `GcBif` tries arith **then** comparison; `Bif` tries only comparison
(the settled corpus emits the eight comparison ops as non-collecting `Bif`).

## Restricted shapes (where each is enforced)
- **Operand readers** (`operands.rs`): `x_register` accepts only `X(n)` / `Typed{reg:X(n)}` (a `Y`
  reg, float reg or literal -> `None`); `constant_word` accepts only `Integer`/`Character`
  (small, via `small_word`) and `Atom`/`Nil` — a bignum constant, a float or a literal -> `None`.
  `MAX_REGISTER` is enforced per lowering (a register past it must deopt so the interpreter reports
  the faithful `bad_register`).
- **`test`** (`test.rs` section comment): word tests `is_nil`/`is_atom`/`is_list`/`is_nonempty_list`
  and `is_boolean` (only when the module interned `true`/`false`); the **boxed-term header tests**
  `is_integer`/`is_number`/`is_float`/`is_tuple`/`is_map`/`is_bitstring`/`is_binary`/`is_function`/
  `is_function2`/`is_tagged_tuple`/`test_arity`/`is_pid`/`is_port`/`is_reference` (each a pure
  tag/header check over one `mov` of the boxed word, mirroring the matching `beam_term` predicate);
  `is_eq_exact`/`is_ne_exact` with a constant operand or two guarded smalls; `is_eq`/`is_ne`;
  `is_lt`/`is_ge` **only smalls**. `NO_FAIL` -> `None`; unresolved fail label -> `None`; an arity
  operand outside the interpreter's range, a module with neither `true` nor `false` interned for
  `is_boolean`, an out-of-bound register, or a bignum/float/literal operand -> `None` (the helper
  then reproduces the interpreter's exact result).
- **`select_val`** (`select.rs`): `NO_FAIL` -> `None`; unresolved label -> `None`; scrutinee must be
  `test_src`; **every** alternative must be an immediate `constant_word`.
- **`select_tuple_arity`** (`aggregate.rs`): `NO_FAIL`/unresolved label -> `None`; scrutinee must be an
  in-bound x-register; every alternative must be a non-negative `Operand::Integer`.
- **Cons/tuple readers** (`aggregate.rs`): source and every destination must be in-bound x-registers;
  the guard is `is_list` / `is_tuple` and can only ever *reject* (a non-cons/non-tuple source, or an
  out-of-range element, branches to the helper, whose interpreter step raises the exact
  `{badmatch,Source}` or the typed `unsupported`). An arity-switch plan on a non-tuple deopts the same way.
- **arith** (`arith.rs::fast_arith`): only `erlang` `+`/`-`/`*` at arity 2, `args.len() == 2`, in-bound
  x destination, every operand loadable (in-bound x or small-int constant).
- **compare** (`compare.rs::comparison_shape`): only the eight `erlang` comparison ops at arity 2; both
  operands must be `cmp_operand`; a derivable guard; and the module must have both `true` and `false`
  interned (else the whole instruction deopts — a wrong atom is never fabricated). `Fail` is
  deliberately not inspected (the fast path never errors; a guard rejection deopts to the full step).

## Deopt mechanism (two levels)
1. **Compile-time shape refusal**: a `try_inline_*` returns `Ok(false)` **without emitting a byte**, and
   the dispatch arm emits `mod.rs::emit_step_and_continue` in its place; the `_` arm does the same for
   any unclaimed `Instr`.
2. **Run-time guard rejection**: an inlined sequence that passed the shape test but whose *runtime* guard
   fails branches to a local `fallback` label that runs `emit_step_and_continue` and then falls through
   to the rest of the block (`fast_done`). Present in `arith.rs`, `compare.rs`, `test.rs`,
   `aggregate.rs` (`emit_list_read`/`emit_tuple_read`/`try_inline_select_tuple_arity`). `select_val` has
   no fallback (all-immediate alternatives are total).
`emit_step_and_continue` (`mod.rs`) = one `jit_step_one` call + `emit_continue_check`: `rax` is compared
against `encode_target(func, pc+1)`; equal -> `fill` and fall through natively at `pc+1`; any other word
(`Goto`/`HALT`/`ERROR`/`ENTER`/`Wait`) -> `jmp done`, the shared epilogue, which `ret`s to the driver with
the word in `rax`. So a non-`Next` helper outcome and the return-to-driver deopt share one epilogue.
`emit_ext_call` (`mod.rs`, the `CallExt*` arm) is the **second** caller of that same check: it emits an
`emit_helper_call` to `jit_ext_call` for the instruction itself — import resolution plus the variant's
frame discipline, no interpreter step — and then `emit_continue_check`. Its `Next` case is the helper's
`jit_step_one` fallback (the synchronous `call_bif` path); its other word is the `ENTER` of a resolved
cross-module transfer, which the driver decodes into the context switch.
`link.rs::emit_exit`'s unlinkable path is a **different** mechanism: it is *not* a helper call — it spills,
materialises the return word in `rax`, and jumps to `done`; the **driver** then looks up the block or
takes one interpreter step.

## Spill/fill discipline
- Prologue (`mod.rs::emit_block`): push `SAVED_REGISTERS` (6 callee-saved regs), reserve the 24-byte `rt`
  slot, `rbx = rt->x_regs`, call `jit_set_position(rt, func, block_start_pc)` (the block-entry position
  store — emitted before the cached loads, because `rbx` is callee-saved and no cached register may sit in
  caller-saved scratch across the call), then load each cached register from `machine.x`.
- `mod.rs::spill` / `mod.rs::fill`: store/load every `(x, reg)` in `Allocation::slots`.
- **Every helper call spills first.** `mod.rs::emit_helper_call` (used by `emit_step_and_continue` and
  `emit_ext_call`) calls `spill` before loading `rdi` from the `rt` slot, filling the ABI argument
  registers `rsi`/`rdx`/`rcx`/`r8`/`r9` with the caller's argument words, and `call r11`; it does **not**
  spill after the call (the helper legitimately wrote `machine.x`) — `fill` reloads instead.
  `link.rs::emit_tail_exit` spills before `call jit_tail_call`. No emitted path in this node calls a helper
  without spilling first.
- `link.rs::emit_exit` and `link.rs::emit_tail_exit` spill on **both** the linked and the deopt paths.
- The epilogue (`mod.rs`: `done` -> `emit_frame_teardown` -> `ret`) does **not** spill: every path reaching
  it has already spilled. The inline fast paths (`move`/`arith`/`compare`/`test`/`select`/`aggregate`) do
  **not** spill themselves — a result lands in a cache register or straight in `machine.x` (the very slot
  the exit spill writes), and the next helper call or block exit spills it.

## Register picker (`regalloc.rs::allocate_registers`)
Usage-weighted, not liveness-based: `cacheable_touches` charges one *native touch* per x-register operand
**of each instruction the tier would actually inline** (it mirrors each lowering's own shape test, so
regalloc and `emit_block` agree). Counts are ranked by descending touch count, ties by ascending register
index, and the top `CACHE_REGISTERS.len()` = 5 are kept (`r12`,`r13`,`r14`,`r15`,`rbp`). Physical
assignment is by **first-appearance order** of the selected registers (deterministic), not by rank. An
out-of-bound register is never counted. Every non-selected register is read/written directly in `machine.x`.

## Constraints
- `#![allow(unsafe_code)]` at the emitter entry (`mod.rs`) because it drives the encoder; nothing here
  dereferences a raw pointer itself.
- `emit_block` accumulates `highest_native_register` (direct `machine.x` accesses) via `touch`; the driver
  refuses native entry to a machine whose register file does not cover it. `compile_module` also records
  `highest_cached_register` / `cached_registers`.
- `CodeBuffer` reservations are budgeted by `code_capacity_estimate` (loose on purpose: under-estimating
  turns a large module into a `SystemLimit` compile error). Block entry labels are reserved **before**
  emitting, so forward branches are ordinary `rel32` fixups, never post-`seal` patching (W^X unchanged).
- A resolved `CallExt`/`CallExtOnly`/`CallExtLast` surfaces from the `emit_ext_call` helper as the `ENTER` word with the resolved `(context, func, pc)` in the runtime's `enter` slot; the **driver** performs the switch (`Machine::switch_context` + module/label/literal re-bind + the callee's blocks via `NativeCode::module_blocks`) and re-enters native code. The transfer is still a driver round-trip (a cross-module link is unsound — separate mappings, buffer-local `rel32` fixups, a module baked into `JitRuntime`), but the *call instruction* no longer costs an interpreter step; a seam miss (the synchronous `call_bif` path) or a failed frame discipline falls back to `jit_step_one` inside the helper.
- Never hand-edit generated data. Files ~1000 lines; split by responsibility. Apache-2.0 header on every
  `.rs` (byte-exact, license scanner enforced).

## Routing Table
- `mod.rs` -> compiled-block shape: `SAVED_REGISTERS`/`CACHE_REGISTERS`/scratch constants, `touch`/`Allocation`,
  `compile_module`, `code_capacity_estimate` + size budgets, `emit_block` (the dispatch), `emit_helper_call`,
  `emit_step_and_continue`, `emit_ext_call`, `emit_continue_check`, `spill`/`fill`.
- `regalloc.rs` -> `allocate_registers` + `cacheable_touches`.
- `operands.rs` -> `x_offset`, `x_register`, `constant_word`, `small_word`.
- `r#move.rs` -> `try_inline_move`, `try_inline_swap`.
- `test.rs` -> `TestSrc`/`TestPred`/`Guard`/`TestPlan`, `test_plan`, `test_src`, `compare_shape`,
  `arith_guard`, `order_guard`, `load_test_src`, `try_inline_test` (also `TEST_A`/`TEST_T` reused by `select.rs`).
- `select.rs` -> `SelectPlan`, `select_val_plan`, `try_inline_select_val`.
- `arith.rs` -> `SmallArith`, `fast_arith`, `import_names`, `try_inline_gc_bif` (+ the small-int constants).
- `compare.rs` -> `ComparisonOp`, `comparison_shape`, `cmp_operand`, `comparison_guard`, `try_inline_comparison`.
- `aggregate.rs` -> `ListShape`/`get_*_shape`/`emit_list_read`, `TupleReadShape`/`get_tuple_element_shape`/
  `emit_tuple_read`, `AritySwitchPlan`/`select_tuple_arity_plan`/`try_inline_select_tuple_arity`,
  `emit_tuple_header_guard`.
- `link.rs` -> `Linker` (`is_compiled`, `emit_exit`, `emit_tail_exit`), `emit_frame_teardown`, and the
  byte-level linker unit tests (the only tests in this node).
- `../runtime.rs` -> `jit_step_one` / `jit_tail_call` / `JitRuntime` / the `encode_target`/`encode_wait` codec
  (the helpers these emitters call).

## Notes for Agents
- Read `super::CONTEXT.md` (`../`) and the crate `CONTEXT.md` (`../../`) for the block ABI, the driver, the
  inline set and the measured figures before changing a narrowing rule.
- The invariant to preserve when adding a fast path: (a) the compile-time shape test must be *purely
  syntactic* and mirrored in `regalloc.rs::cacheable_touches`, or regalloc and `emit_block` disagree;
  (b) a run-time guard may only ever *reject* (branching to `emit_step_and_continue`), never fabricate a
  result; (c) any value left in a cache register is covered by `spill` at the next helper call / block exit.
- `baseline/tests.rs` glob-imports `emit`; `mod.rs` re-exports some items under `#[cfg(test)]` so splitting
  the emitter into files did not change item paths. Keep that in mind when renaming items used by tests.
