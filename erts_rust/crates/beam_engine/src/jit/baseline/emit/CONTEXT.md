# `emit` -- Baseline JIT x86-64 Instruction Emitters

## Intent
Lower the typed IR basic blocks of a `beam_loader::Module` into native x86-64 functions for the
**baseline** execution tier.
One file per responsibility: `mod.rs` owns the compiled-block shape (prologue/epilogue,
`emit_block`'s lowering dispatch, the deopt sequence); `regs.rs` owns the register cache
(`Allocation`/`spill`/`fill`/`SpillSite`/`FillSite`); `compile.rs` owns the compilation entry points
— `compile_module` (the **eager** whole-module layout: one shared `CodeBuffer`, a whole-module block
map) and `compile_function`/`emit_function` (the **lazy** layout's per-function unit: its own
`CodeBuffer`, a map covering only that function) — plus the continuation sub-blocks and the code-size
estimates; `regalloc.rs` picks the cached x-registers and derives each block's Y/refresh gates;
`liveness.rs` computes the per-pc fill sets; `operands.rs` reads operand shapes and owns the shared
guarded-Y access plus the `Loc` location abstraction; `r#move.rs`/`test.rs`/`select.rs`/`arith.rs`/
`compare.rs`/`aggregate.rs` each lower one instruction group; `frame.rs` lowers the frame-discipline
trio that ends a function body (`deallocate`/`return`/`deallocate_return`); `heap.rs` lowers the heap
/ frame-admission family (`allocate`/`allocate_heap`/`test_heap`/`trim`); `family.rs` emits the shared
helper arm for every instruction the interpreter routes to `crate::families::dispatch`; `link.rs`
owns the direct block-to-block jump, the linked local tail call and the unlinked tail-call exit.
The driver, block ABI and the helpers (`jit_step_one`, `jit_ext_call`, `jit_local_call`,
`jit_call_store`, `jit_tail_call`, `jit_deallocate`, `jit_return`, `jit_deallocate_return`,
`jit_allocate`, `jit_allocate_heap`, `jit_test_heap`, `jit_trim`, `jit_receive`, `jit_family`,
`jit_set_position`, `jit_publish_y`, `jit_refresh_y`) live one level up in `../mod.rs`,
`../runtime.rs`, `../call.rs`, `../receive.rs` and `../family.rs`; this node only *emits calls* to
them.

## Core model (do not break)
- **Only `CACHE_REGISTERS.len()` = 5 x-registers are held in machine registers** (`r12`-`r15` + `rbp`,
  `mod.rs` `CACHE_REGISTERS`); every other x-register an instruction names is addressed **directly in
  `machine.x`** via `[rbx + x*8]` (`rbx` = `rt->x_regs`, loaded in the prologue).
  A register is either *always* cached in its machine register or *never* cached, fixed for the whole
  block, so the two representations can never disagree.
- **Both directions of the register cache are partial.** `spill` stores only the block's **written**
  set — the cached x-registers some inline lowering of the block may write (`Allocation::written`, a
  static superset populated by `regalloc.rs::cacheable_writes`); `fill` reloads only
  `Allocation::fill_slots(pc)` — the registers the continuation can still observe, the block-local
  liveness of `liveness.rs::fill_sets` unioned with the write set, `(live ∪ written) ∩ slots` — except
  the prologue load, which is always the whole cache.
  The cache itself lives in `regs.rs` (`Allocation`/`spill`/`fill`); the call sites are in `mod.rs`.
- Caller-saved scratch registers, used only *within* one instruction's lowering: `MOVE_SCRATCH`=`r11`,
  `MOVE_SCRATCH_2`=`r10` (`mod.rs`); `Y_SCRATCH`=`r9` and `SWAP_END`=`rax` (`r#move.rs`);
  `FAST_A`=`rax`, `FAST_B`=`r8`, `FAST_C`=`rcx`, `FAST_T`=`r10`, `FAST_R`=`r11` (`arith.rs`);
  `TEST_A`=`rax`, `TEST_B`=`r8`, `TEST_C`=`r9`, `TEST_T`=`r10`, `TEST_H`=`r11` (`test.rs`);
  `GUARD_SCRATCH`=`r8` (`aggregate.rs`); `Y_ADDR_SCRATCH`=`r9` / `Y_ADDR_SCRATCH_2`=`r8`
  (`operands.rs`, the pre-computed Y destination addresses for a multi-destination writer).
  None overlaps a cache register, `rbx` or `rsp`.

## The lowering dispatch (the compile-time inlinable-vs-deopt decision)
`mod.rs::emit_block` matches each `Instr` of the block (the `match instr { .. }`).
Each arm calls the group's `try_inline_*`; **`Ok(false)` means "lowered nothing"** and the arm then
emits `emit_deopt`.
Whole-instruction inlining therefore is a **compile-time, syntactic** decision (never a value test).
Logical groups:

| Group | Instr(s) | Lowering entry | Shape/guard predicate |
|---|---|---|---|
| Metadata no-ops | `Label`,`Line`,`IntCodeEnd`,`ExecutableLine`,`DebugLine` | `mod.rs` empty arm (no bytes) | n/a |
| Move / swap | `Move`, `Swap` | `r#move.rs::try_inline_move`, `::try_inline_swap` | `r#move.rs::move_shape`: source = in-bound `X(n)`, in-bound **`Y` slot**, a constant word or a **literal-area** term (`MoveValue::Literal`); destination = in-bound `X(n)` or an in-bound **`Y` slot** (a `Y` side and a literal source each carry a run-time guard). `swap_shape` accepts two `Loc` operands and lowers a `Y` side via `emit_swap_y` |
| Y initialization | `InitYregs` | `mod.rs::try_inline_init_yregs` | `operands.rs::locate_all` — every listed operand an in-bound `X(n)` or an addressable `Y` slot |
| Test (word + boxed header) | `Test` | `test.rs::try_inline_test` | `test.rs::test_plan` (a `TestSrc::Y` operand carries a run-time guard) |
| Switch | `SelectVal` | `select.rs::try_inline_select_val` | `select.rs::select_val_plan` (a `TestSrc::Y` scrutinee carries a run-time guard) |
| Arity switch | `SelectTupleArity` | `aggregate.rs::try_inline_select_tuple_arity` | `aggregate.rs::select_tuple_arity_plan` (the tuple operand is a `Loc`) |
| Cons readers | `GetList`,`GetHd`,`GetTl` | `aggregate.rs::try_inline_get_{list,hd,tl}` via `emit_list_read` | `get_{list,hd,tl}_shape` -> `shape`; source and every destination are a `Loc` |
| Tuple read | `GetTupleElement` | `aggregate.rs::try_inline_get_tuple_element` via `emit_tuple_read` | `get_tuple_element_shape` (source and destination are a `Loc`) |
| Tuple write | `SetTupleElement` | `aggregate.rs::try_inline_set_tuple_element` via `emit_tuple_write` | `set_tuple_element_shape` (the tuple is a `Loc`) |
| Arithmetic gc_bif | `GcBif` (`+`/`-`/`*`) | `arith.rs::try_inline_gc_bif` | `arith.rs::fast_arith` (x-register **or Y-slot** operand/destination; a Y access carries a run-time frame-window guard) |
| Comparisons | `Bif` (and `GcBif` fallback) | `compare.rs::try_inline_comparison` | `compare.rs::comparison_shape` |
| Frame pop | `Deallocate` | `frame.rs::emit_deallocate` (an `emit_helper_call` to `jit_deallocate`) | always lowered (the instruction's own outcome is the helper's block-ABI word): continues in-block on `encode_target(func, pc+1)`, else leaves via `done` |
| Return | `Return` | `frame.rs::emit_return` (an `emit_helper_call` to `jit_return`) | always lowered — a `return` always transfers control, so the block leaves via `emit_word_exit` |
| Fused frame pop + return | `DeallocateReturn` | `frame.rs::emit_deallocate_return` (an `emit_helper_call` to `jit_deallocate_return`); `emit_block` also fuses a separate adjacent `Deallocate`+`Return` pair into one such call | always lowered — always transfers control, leaves via `emit_word_exit` |
| Frame push | `Allocate` | `heap.rs::emit_allocate` (an `emit_helper_call` to `jit_allocate`) | always lowered (the helper's `Machine::push_frame`; `allocate` performs no heap admission): continues in-block on `encode_target(func, pc+1)`, else leaves via `done` |
| Frame push + heap admission | `AllocateHeap` | `heap.rs::emit_allocate_heap` (an `emit_helper_call` to `jit_allocate_heap`) | always lowered — one helper round-trip does the interpreter's two calls in order (`ensure_heap` **then** `push_frame`); continues in-block via the shared `emit_continue_check` (partial refill + gated Y refresh) |
| Heap admission | `TestHeap` | `heap.rs::emit_test_heap` (an `emit_helper_call` to `jit_test_heap`) | always lowered — reaches the host's admission door exactly once with the instruction's own `heap_need`/`live`; continues in-block via `emit_continue_check` |
| Frame trim | `Trim` | `heap.rs::emit_trim` (an `emit_helper_call` to `jit_trim`) | always lowered — an over-trim / frameless trim is the helper's `ERROR`; continues in-block on `encode_target(func, pc+1)` |
| Jump | `Jump` | `link.rs::Linker::emit_exit` | resolved label **in the block map** -> linked; otherwise return-to-driver (the target's function is compiled on entry) |
| Local tail call | `CallLast` | `mod.rs::emit_local_call_probe` then `link.rs::Linker::emit_tail_exit` (compiled successor) or `::emit_tail_exit_to_driver` (resolved but uncompiled successor) | `Linker::is_compiled`; an undefined label keeps the `emit_deopt` `unknown_label` |
| Local tail call (no pop) | `CallOnly` | `mod.rs::emit_local_call_probe` then `link.rs::Linker::emit_exit` | resolved label |
| Local frame-preserving call | `Call` | `mod.rs` `emit_helper_call(jit_call_store)` + a `CALL_STORED` compare, then `link.rs::Linker::emit_exit` | resolved label; the helper stores the continuation *and* consults the local-call seam |
| Cross-module call | `CallExt`,`CallExtOnly`,`CallExtLast` | `mod.rs::emit_ext_call` (an `emit_helper_call` to `jit_ext_call`) | n/a — the helper resolves the import and reports `ENTER` |
| Receive/send control | `Send`,`LoopRec`,`LoopRecEnd`,`Wait`,`WaitTimeout`,`Timeout`,`RemoveMessage` | `mod.rs::emit_receive` (an `emit_helper_call` to `jit_receive`) | n/a — the helper runs the family body and reports the block-ABI word |
| Family-owned surface | `PutList`,`PutTuple2`, the native-record set, `MakeFun3`,`CallFun`,`CallFun2`,`Apply`,`ApplyLast`, the try/catch/raise set, the float set, the map set, the bitstring set, `RecvMarker*` | `family.rs::emit_family` (an `emit_helper_call` to `jit_family`) | n/a — the helper runs the owning family's own body and reports the block-ABI word |
| Everything else | -- | `_` arm -> `emit_deopt` | -- (`FuncInfo` is deliberately here, never a no-op) |

`emit_block`'s family arm names every variant `crate::families::dispatch` claims and the core arms
above do not: `put_list`/`put_tuple2`, `update_record`/`is_any_native_record`/`is_native_record`/
`get_record_elements`/`get_record_elements_id`/`put_record`/`is_record_accessible`/`get_record_field`/
`update_record_id`, `make_fun3`/`call_fun`/`call_fun2`/`apply`/`apply_last`,
`try`/`catch`/`badmatch`/`case_end`/`try_case_end`/`badrecord`/`if_end`/`raise`/`try_case`/`try_end`/
`catch_end`/`build_stacktrace`/`raw_raise`, `fmove`/`fconv`/`fadd`/`fsub`/`fmul`/`fdiv`/`fnegate`,
`get_map_elements`/`has_map_fields`/`put_map_assoc`/`put_map_exact`, the `bs_*` bitstring family and
`recv_marker_reserve`/`_bind`/`_use`/`_clear`.
It routes each to `jit_family`, which runs the identical `crate::families::dispatch` call the
interpreter's per-instruction step would, so the arm can never produce a different result than the
step it replaces.
Note `GcBif` tries arith **then** comparison; `Bif` tries only comparison (the settled corpus emits
the eight comparison ops as non-collecting `Bif`).

## Restricted shapes (where each is enforced)
- **Operand readers** (`operands.rs`): `x_register` accepts only `X(n)` / `Typed{reg:X(n)}` (a `Y`
  reg, float reg or literal -> `None`); `y_register` is its Y twin (a purely syntactic `Y(n)` /
  `Typed{reg:Y(n)}` read — the caller decides addressability via `y_addressable`); `y_offset` is the
  `lea [y_base + k*8]` displacement. `constant_word` accepts only `Integer`/`Character`
  (small, via `small_word`) and `Atom`/`Nil` — a bignum constant, a float or a literal -> `None`.
  `MAX_REGISTER` is enforced per lowering (a register past it must deopt so the interpreter reports
  the faithful `bad_register`); `y_addressable` refuses a Y index whose `disp32` would overflow
  (`k <= i32::MAX / size_of::<Eterm>()`). `Loc::of`/`locate_all` build the wider location set (an
  in-bound `X(n)` or an addressable `Y` slot) the widened lowerings share.
- **`test`** (`test.rs` section comment): word tests `is_nil`/`is_atom`/`is_list`/`is_nonempty_list`
  and `is_boolean` (only when the module interned `true`/`false`); the **boxed-term header tests**
  `is_integer`/`is_number`/`is_float`/`is_tuple`/`is_map`/`is_bitstring`/`is_binary`/`is_function`/
  `is_function2`/`is_tagged_tuple`/`test_arity`/`is_pid`/`is_port`/`is_reference` (each a pure
  tag/header check over one `mov` of the boxed word, mirroring the matching `beam_term` predicate);
  `is_eq_exact`/`is_ne_exact` with a constant operand or two guarded smalls; `is_eq`/`is_ne`;
  `is_lt`/`is_ge` **only smalls**. A `TestSrc` is an in-bound x-register, an **addressable `Y` slot**
  (behind the frame-window guard), a small immediate or a raw immediate word; `TestPlan::uses_y`
  gates the block's Y publish/refresh, `TestPlan::emits_guard` reports whether a run-time guard was
  emitted, and `TestPlan::needs_deopt` reports whether the plan's failure path is the interpreter
  step. `NO_FAIL` -> `None`; unresolved fail label -> `None`; an arity operand outside the
  interpreter's range, a module with neither `true` nor `false` interned for `is_boolean`, an
  out-of-bound register, or a bignum/float/literal operand -> `None` (the helper then reproduces the
  interpreter's exact result).
- **`test` comparison helper** (`test.rs::emit_test_helper`): a comparison `Test` the word/header
  subset declines — the six comparison kinds at two operands with a resolvable non-`NO_FAIL` fail
  label, whatever the operands (a `Y` slot, a float register, a literal-area term, boxed-vs-boxed) —
  is emitted as an `emit_helper_call` to the `jit_test` helper (so the cached x-registers spill
  first), then `cmp rax, TEST_TRUE / je success`, `cmp rax, TEST_FALSE / je failed`, else `jmp done`
  (the helper's `ERROR` word / decline, decoded by the driver). `success` refills, emits the gated
  `emit_refresh_y` when `refresh_y_after(pc)`, and falls through to the next instruction; `failed` is
  `linker.emit_exit` to the resolved fail label, so a compiled fail block is jumped to **directly**
  (its own frame teardown replaces this block's) and any other target returns its `(func, pc)` word
  to the driver. Any shape it does not own emits **zero** bytes, so the caller's fallback is exactly
  the per-instruction `jit_step_one`.
- **`select_val`** (`select.rs`): `NO_FAIL` -> `None`; unresolved label -> `None`; the scrutinee must
  be a `TestSrc` (an in-bound x-register, an addressable `Y` slot, or a constant); **every**
  alternative must be an immediate `constant_word`. `SelectPlan::uses_y` gates the Y publish/refresh;
  a Y scrutinee emits a frame-window guard whose rejection runs one `emit_step_and_continue`.
- **`select_tuple_arity`** (`aggregate.rs`): `NO_FAIL`/unresolved label -> `None`; the tuple operand
  must be a `Loc`; every alternative must be a non-negative `Operand::Integer`.
- **Cons/tuple readers** (`aggregate.rs`): the source and every destination must be a `Loc` (an
  in-bound x-register or an addressable `Y` slot); the guard is `is_list` / `is_tuple` and can only
  ever *reject* (a non-cons/non-tuple source, or an out-of-range element, branches to the helper,
  whose interpreter step raises the exact `{badmatch,Source}` or the typed `unsupported`). An
  arity-switch plan on a non-tuple deopts the same way. Multi-destination writers front-load every `Y`
  guard (`Loc::guard_store`) before any store.
- **Tuple writer** (`aggregate.rs::set_tuple_element_shape`): the **target tuple** must be a `Loc`,
  the **element** must be an immediate constant word (`operands.rs::constant_word`) or an in-bound
  x-register — a value needing heap materialisation (a bignum/float/literal operand) is *not*
  a lowered shape and deopts to the interpreter, which materialises it exactly. The lowering is the
  same `is_tuple` header guard the reads use plus a range guard (`offset < arity`) and a
  **literal-area** guard: the interpreter refuses a `TAG_LITERAL_PTR` target with a typed
  `unsupported` (writing shared module constants would corrupt them), so the emitted guard rejects
  that bit too, branching to the helper. The store writes the word `8*(1+offset)` past the header —
  the exact word `get_tuple_element` reads — straight through scratch (no write barrier, no
  cached-register clobber).
- **arith** (`arith.rs::fast_arith`): only `erlang` `+`/`-`/`*` at arity 2, `args.len() == 2`; the
  destination is an in-bound x-register (`FastDest::X`) **or an addressable Y slot** (`FastDest::Y`),
  and every operand is loadable — an in-bound x-register, an **addressable Y slot** (`FastSource::YSlot`)
  or a small-int constant. Every Y access emits a **run-time** frame-window guard (a pure rejection to
  `jit_step_one`); an un-addressable Y index (`operands.rs::y_addressable`) is refused at compile time.
  `FastArith::uses_y` is the emitter's Y-window gate and `FastArith::registers` pushes only x-register
  operands plus an x-register destination, so a Y slot never enters the usage counts.
- **compare** (`compare.rs::comparison_shape`): only the eight `erlang` comparison ops at arity 2; both
  operands must be `cmp_operand`; a derivable guard; and the module must have both `true` and `false`
  interned (else the whole instruction deopts — a wrong atom is never fabricated). `Fail` is
  deliberately not inspected (the fast path never errors; a guard rejection deopts to the full step).

## Deopt mechanism (three levels)
1. **Compile-time shape refusal**: a `try_inline_*` returns `Ok(false)` **without emitting a byte**, and
   the dispatch arm emits `mod.rs::emit_deopt` in its place; the `_` arm does the same for any unclaimed
   `Instr`. `emit_deopt` records the static-census deopt, then runs `emit_step_and_continue`.
2. **Run-time guard rejection**: an inlined sequence that passed the shape test but whose *runtime* guard
   fails branches to a local `fallback` label that runs `emit_step_and_continue` and then falls through
   to the rest of the block (`fast_done`). Present in `arith.rs`, `compare.rs`, `test.rs`,
   `aggregate.rs` (`emit_list_read`/`emit_tuple_read`/`try_inline_select_tuple_arity`), `select.rs`, and
   every guarded Y access (`operands.rs::emit_guarded_y_address`/`_load`/`_store`). `select_val`
   has no fallback (all-immediate alternatives are total).
3. **Direct-helper exit (not a deopt at all)**: the frame-discipline arms (`frame.rs` —
   `Deallocate`/`Return`/`DeallocateReturn`), the heap / frame-admission family (`heap.rs` —
   `Allocate`/`AllocateHeap`/`TestHeap`/`Trim`), the receive/send family (`emit_receive`), the
   family-owned surface (`family.rs::emit_family`) and the local frame-preserving `Call`
   (`jit_call_store`) are lowered as a plain `emit_helper_call` to a dedicated helper that performs
   exactly this instruction's `Machine` work and hands the driver a block-ABI word, so **no
   interpreter step runs**: `emit_deallocate`, `heap.rs::emit_allocate`/`emit_trim` and the
   `CALL_STORED` arm continue in-block on the appropriate word (branching to `done` otherwise), while
   `emit_return`/`emit_deallocate_return` always transfer control and leave via `emit_word_exit`;
   `heap.rs::emit_allocate_heap`/`emit_test_heap`, `emit_ext_call`, `emit_receive` and `emit_family`
   continue through the shared `emit_continue_check` (a collecting host's admission rewrites
   `machine.x`, so the cache is refilled before the block resumes). There are no guards to emit: every
   operand shape is lowered (a frame size / heap need / live count / trim `n` is a `u32` the helper
   takes as an argument, and a `return` has no operand), and the only failure is a `CodeBuffer` that
   cannot hold the bytes — the shared typed `SystemLimit`.

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
materialises the return word in `rax`, and jumps to `done`; the **driver** then looks the target up in the
active artifact — for a lazy artifact a branch to another function is simply absent from that function's
block map, so the driver compiles the target's function on entry — or takes one interpreter step.
`link.rs::emit_tail_exit_to_driver` is the `CallLast` sibling of that path: it runs the interpreter's
checked pop (`jit_tail_call`) and then leaves the **resolved** `(func, pc)` word in `rax`, which is the
interpreter's `Step::Goto` for a tail call into a function this compilation emitted no block for.

## heap / frame-admission lowerings (`heap.rs`)

`heap.rs` lowers the four instructions that touch the two pieces of `Machine` state a compiled block
cannot address itself — the **call-frame stack** (`Machine::frames`/`stack`, Rust-side `Vec`s with no
layout a block could compute an offset into) and the **process heap** (the host's
`ExecutionContext::ensure_heap_full` admission door, which may run a Stage-4 collection): `allocate`,
`allocate_heap`, `test_heap`, `trim`.
It models `frame.rs` — the same `emit_helper_call` + block-ABI-word pattern as the frame-discipline
trio — and one helper (`jit_allocate`, `jit_allocate_heap`, `jit_test_heap`, `jit_trim` in
`../runtime.rs`) performs exactly the instruction's own `Machine` calls in the interpreter's own
order, so **no interpreter step runs**: the instruction costs one helper call, never a `jit_step_one`
deopt.

All four are **non-terminating** (the interpreter's arm returns `Step::Next`): the emitted code compares
the helper's word with `encode_target(func, pc+1)` and continues in-block on a match, or branches to
`done` with the word in `rax` on anything else (always the `ERROR` of a failed admission / a failed
trim). The four split into **two shapes**, by whether the helper can touch `machine.x`:

- **frame-only** — `allocate`/`trim` (`emit_allocate`/`emit_trim`) push/shrink the frame and touch **no**
  x-register, so, like `frame.rs::emit_deallocate`, they emit **no** register refill (the block's cached
  registers still hold exactly what `machine.x` does); because they *are* frame changes they re-publish
  the Y window on the continue arm — the private `emit_frame_change_continue` (the shared
  compare-and-branch **without** a refill), gated on `Allocation::refresh_y_after(pc)`.
- **heap-admitting** — `allocate_heap`/`test_heap` (`emit_allocate_heap`/`emit_test_heap`) reach the
  host's admission door, and a **collecting** host relocates the young heap and rewrites every root **in
  place** (including `machine.x`), so the block's cached x-registers are stale the instant the helper
  returns: both continue through the shared `emit_continue_check`, whose partial refill *is* the
  post-admission contract, and whose gated Y refresh covers the frame change. `allocate_heap`'s single
  round-trip does the interpreter's two calls in order (`ensure_heap` **then** `push_frame`).

**There are no guards to emit** — every operand shape is lowered (each of `stack_need`/`heap_need`/`live`
is a `u32` the helper takes as an argument, computed at compile time by `HeapNeed::words()`, and `trim`'s
`n` likewise) — so the only failure is a `CodeBuffer` that cannot hold the emitted bytes, the shared typed
`SystemLimit`. The two heap-admitting lowerings reach the host's admission door **exactly once** with the
instruction's own operands.

## Spill/fill discipline
- Prologue (`mod.rs::emit_block`): push `SAVED_REGISTERS` (6 callee-saved regs), reserve the 24-byte `rt`
  slot, `rbx = rt->x_regs`, then call `jit_set_position(rt, func, block_start_pc)` (the block-entry
  position store — emitted before the cached loads, because `rbx` is callee-saved and no cached register
  may sit in caller-saved scratch across the call) — or, when the block accesses a Y slot
  (`Allocation::y_last`), `jit_publish_y(rt, func, pc)`, which does that same position store **and**
  re-publishes the current frame's Y window in one call — then load each cached register from `machine.x`.
- `regs.rs::spill` / `regs.rs::fill`: `spill` stores every `(x, reg)` in `Allocation::spill_slots()`
  (the **written** subset of `Allocation::slots`); `fill` loads every `(x, reg)` in
  `Allocation::fill_slots(pc)` — the continuation's block-local liveness unioned with the write set —
  except the prologue load, which is the whole cache (see the **Register cache** section below).
- **Every helper call spills first.** `mod.rs::emit_helper_call` (used by `emit_step_and_continue`,
  `emit_ext_call`, `emit_receive`, `emit_family`, the `Call` arm, the frame-discipline trio, the
  heap/frame-admission family and the comparison-`test` helper) calls `spill` before loading `rdi` from
  the `rt` slot, filling the ABI argument registers `rsi`/`rdx`/`rcx`/`r8`/`r9` with the caller's
  argument words, and `call r11`; it does **not** spill after the call (the helper legitimately wrote
  `machine.x`) — `fill` reloads instead. `link.rs::emit_tail_exit` spills before `call jit_tail_call`.
  No emitted path in this node calls a helper without spilling first.
- **Gated Y-window refresh after an in-block helper.** `emit_continue_check` (the shared continue arm of
  `emit_step_and_continue`, `emit_ext_call`, `emit_receive` and `emit_family`) emits `jit_refresh_y(rt)`
  — after `fill`, on the continue-in-block path only — exactly when a later instruction of the block
  still accesses a Y slot (`Allocation::refresh_y_after(pc)`), because the helper ran one instruction in
  the interpreter, which may have pushed/`trim`med a frame or **reallocated** `machine.stack` and so
  staled the prologue's window. When no later instruction accesses Y the call is not emitted at all, and
  a helper that leaves the block to the driver pays nothing (the next block's prologue publishes).
- `link.rs::emit_exit` and `link.rs::emit_tail_exit` spill (the written set) on **both** the linked and
  the deopt paths.
- The epilogue (`mod.rs`: `done` -> `emit_frame_teardown` -> `ret`) does **not** spill: every path reaching
  it has already spilled. The inline fast paths (`move`/`arith`/`compare`/`test`/`select`/`aggregate`) do
  **not** spill themselves — a result lands in a cache register or straight in `machine.x` (the very slot
  the exit spill writes), and the next helper call or block exit spills it (its destination register is in
  the block's written set, so that spill stores it).

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

`allocate_registers` also derives `Allocation::y_last` — the highest pc of the block whose instruction
accesses a Y slot (`arith::uses_y`, the union of every lowering's per-shape Y test) — which is the
compile-time gate for the emitter's prologue publish and its post-helper refresh. A `Y` slot is **not** an
x-register, so it is neither a candidate nor charged to any live interval, and it never raises
`highest_native_register`: a stack slot must not refuse native entry (the driver's bound is
`machine.x.len() > bound`).

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

## `Call` — the frame-preserving local call and `CALL_STORED`
`Instr::Call` is the one local call form that writes a continuation; `jit_call_store` does that store
*and* consults the host's local-call seam (`ExecutionContext::resolve_local_call`) in one helper call, in
the interpreter's own probe-then-store order (a served callee's own code never runs and must not be left
a return address).
Its answer is a block-ABI word: `CALL_STORED` (`-5`) means "the continuation is in place, transfer now",
and every other word (a served callee's folded `Step`, or the interpreter's own typed diagnostic) leaves
the block for the driver.
`CALL_STORED` is consumed **inside** the block (the arm compares `rax` against it and branches to `done`
on a mismatch); it is disjoint from `HALT`/`ERROR`/`ENTER`/`LOCAL_DECLINE`/`WAIT_BIT` and never reaches
the driver's decoder.

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

## Routing Table
- `mod.rs` -> compiled-block shape: `SAVED_REGISTERS`/`CACHE_REGISTERS`/scratch constants, `touch`,
  `emit_block` (the dispatch), `try_inline_init_yregs`, `emit_position_store`/`emit_publish_y`/
  `emit_prologue_helper`/`emit_refresh_y` (the Y-window prologue publish / gated post-helper refresh),
  `emit_helper_call`, `emit_step_and_continue`, `emit_deopt`, `emit_local_call_probe`, `emit_ext_call`,
  `emit_receive`, `emit_continue_check`; re-exports `regs::{Allocation, SpillSite, FillSite}` and
  `compile_module`/`compile_function` (keeping those item paths stable for `baseline/mod.rs` and
  `baseline/code.rs`) plus the `#[cfg(test)]` names `baseline/tests.rs` reaches through `emit::*`.
- `regs.rs` -> the block register cache: `Allocation` (`slots`/`written`/`y_last`/`block_start`/
  `fill_sets`, `slot`/`is_written`/`spill_slots`/`fill_slots`/`refresh_y_after`), `spill` (stores only
  `written`), `fill` (loads only `fill_slots(pc)`, else the whole cache at the prologue), and the
  test-only `SpillSite`/`FillSite` classification of the call sites; `liveness.rs` computes the per-pc
  `fill_sets`.
- `compile.rs` -> the compilation entry points and what they share: `compile_module` (**eager**:
  whole-module block map + the merged whole-module summary that guards entry, plus `emit_shared_sub_blocks`)
  and `compile_function` (**lazy**: one function's own buffer/entries/sub_entries/summary, sharing
  `emit_function` and running `emit_sub_blocks`), `stores_continuation`, `emit_sub_blocks`/
  `emit_shared_sub_blocks`, `emit_function`, and the code-size budgets (`BLOCK_BYTES`/`INSTRUCTION_BYTES`/
  `FAST_PATH_BYTES`/`COMPARE_PATH_BYTES`/`LIST_PATH_BYTES`/`TUPLE_PATH_BYTES`) with their estimates
  `code_capacity_estimate`/`function_capacity_estimate`/`sub_blocks_capacity_estimate`.
- `regalloc.rs` -> `allocate_registers` (incl. the `y_last` and `written` derivations) + `cacheable_touches`
  (the per-instruction native touches) + `cacheable_writes` (the block's written x-registers, mirroring each lowering's shape
  test but returning destinations).
- `operands.rs` -> `x_offset`, `x_register`, `y_offset`, `y_register`, `y_addressable`, `constant_word`,
  `small_word`, the shared guarded-Y access (`emit_guarded_y_address`/`emit_guarded_y_load`/
  `emit_guarded_y_store`/`guard_y_store`), the `Loc` location abstraction
  (`of`/`is_y`/`register`/`load`/`guard_store`/`store_at`), `locate_all`, and the
  `Y_ADDR_SCRATCH`/`Y_ADDR_SCRATCH_2` address scratches.
- `r#move.rs` -> `try_inline_move`, `try_inline_swap` (`Move` over in-bound x-registers, an addressable
  `Y` slot on either side behind the frame-window guard, a constant word or a **literal-area term**
  behind the literal guard; `swap` over two `Loc` operands via `emit_swap_y`), `MoveShape`/`move_shape`/
  `MoveValue`/`MoveDest`/`MoveSource`, `SwapShape`/`swap_shape`, `emit_move_literal`/
  `emit_guarded_literal_load` (the literal-area guard), and the `Y_SCRATCH`/`SWAP_END` scratches.
- `test.rs` -> `TestSrc`/`TestPred`/`TestPlan`/`Guard`, `test_plan`, `test_src`, `is_runtime`,
  `compare_shape`, `arith_guard`, `order_guard`, `load_test_src`, `guard_small`, `arity_operand`,
  `try_inline_test` (incl. its `emit_test_helper` comparison fallback), `emit_test_helper` (also
  `TEST_A`/`TEST_T` reused by `select.rs`); `test/refusal_census.rs` -> the test-only refusal
  attribution.
- `select.rs` -> `SelectPlan`, `select_val_plan`, `try_inline_select_val`.
- `arith.rs` -> `SmallArith`, `fast_arith`, `FastArith`/`FastDest`/`FastSource`, `uses_y`, `import_names`,
  `try_inline_gc_bif`, `emit_y_load`/`emit_y_store` (+ the small-int constants).
- `compare.rs` -> `ComparisonOp`, `comparison_shape`, `cmp_operand`, `comparison_guard`,
  `try_inline_comparison`.
- `aggregate.rs` -> `ListShape`/`get_*_shape`/`emit_list_read`, `TupleReadShape`/
  `get_tuple_element_shape`/`emit_tuple_read`, `TupleWriteShape`/`TupleWriteValue`/
  `set_tuple_element_shape`/`emit_tuple_write`/`try_inline_set_tuple_element`,
  `AritySwitchPlan`/`select_tuple_arity_plan`/`try_inline_select_tuple_arity`,
  `emit_tuple_header_guard`.
- `link.rs` -> `Linker` (`is_compiled`, `emit_exit`, `emit_tail_exit`, `emit_tail_exit_to_driver`),
  `emit_frame_teardown`, and the byte-level linker unit tests (alongside `test.rs`'s own unit tests).
- `frame.rs` -> `emit_deallocate`, `emit_return`, `emit_deallocate_return`, `emit_word_exit` (the three
  end-of-body lowerings + the shared `jmp done` exit), plus its unit tests (the frame-discipline bytes
  and the emitter-side `Deallocate`+`Return` fusion).
- `heap.rs` -> `emit_allocate`, `emit_allocate_heap`, `emit_test_heap`, `emit_trim` (the heap /
  frame-admission family — a direct `emit_helper_call` to the `../runtime.rs` helpers) and the private
  `emit_frame_change_continue` (the frame-only helpers' continue arm: compare against
  `encode_target(func, pc+1)` then the gated Y refresh, no register refill), plus its unit tests (the
  spill-first call order, the frame-only-vs-heap-admitting refill split, and the Y-window refresh
  gating).
- `family.rs` -> `emit_family` (the shared helper arm for every family-owned variant: an
  `emit_helper_call` to `../family.rs::jit_family` + `emit_continue_check`).
- `../runtime.rs` -> `jit_step_one` / `jit_ext_call` / `jit_local_call` / `jit_tail_call` /
  `jit_deallocate` / `jit_return` / `jit_deallocate_return` / `jit_allocate` / `jit_allocate_heap` /
  `jit_test_heap` / `jit_trim` / `jit_set_position` / `jit_publish_y` / `jit_refresh_y` / `JitRuntime`
  (`y_base`/`y_end`/`literals_base`/`literals_end`, `refresh_y_base`/`refresh_literals`) / the
  `encode_target`/`encode_wait`/`decode_target`/`decode_wait` codec and the
  `HALT`/`ERROR`/`ENTER`/`LOCAL_DECLINE`/`WAIT_BIT` sentinels (the helpers these emitters call).
- `../call.rs` -> `jit_call_store` + `CALL_STORED`/`CallOutcome`; `../receive.rs` -> `jit_receive`;
  `../family.rs` -> `jit_family`.

## Notes for Agents
- Read `super::CONTEXT.md` (`../`) and the crate `CONTEXT.md` (`../../`) for the block ABI, the driver, the
  inline set and the measured figures before changing a narrowing rule.
- The invariant to preserve when adding a fast path: (a) the compile-time shape test must be *purely
  syntactic* and mirrored in `regalloc.rs::cacheable_touches`, or regalloc and `emit_block` disagree;
  (b) a run-time guard may only ever *reject* (branching to `emit_step_and_continue`), never fabricate a
  result; (c) any value left in a cache register is covered by `spill` at the next helper call / block exit.
- `baseline/tests.rs` glob-imports `emit`; `mod.rs` re-exports some items under `#[cfg(test)]` so splitting
  the emitter into files keeps every item path stable. Keep that in mind when renaming items used by tests.
- `mod.rs` is the node's largest file (well past the ~1000-line guidance) and is the next split candidate;
  the natural seams are the deopt/continue helpers and the per-arm inline predicates, which already have
  their own submodules.

## Continuation sub-blocks (both layouts)

A **continuation address** is the `pc + 1` a **frame-preserving call** stores in the live frame — one of
`Instr::Call`, a non-tail `Instr::CallExt`, `Instr::CallFun`, `Instr::CallFun2` and `Instr::Apply`
(`stores_continuation`). The tail forms (`call_last`/`call_only`/`call_ext_only`/`call_ext_last`/
`apply_last`) store no new continuation. The loader cuts a function into blocks only at `Instr::Label`s,
so a continuation address usually lies **inside** an IR block, where the driver would otherwise run the
rest of the block one interpreter-step-per-instruction. **Both** layouts lower one sub-block per such
address, through the same `emit_block` an IR block uses (identical shape rules, guards and deopts):

- **Lazy** — `compile_function` lowers the IR blocks with `emit_function`, then the sub-blocks with
  `emit_sub_blocks`, recording each native entry in `CompiledFunction::sub_entries[p]`.
- **Eager** — `compile_module` lowers every IR block, then calls `emit_shared_sub_blocks` **after every**
  IR block (so the block regions stay contiguous at the front of the shared mapping) and **before**
  `seal`, recording each entry in `EagerLayout::sub_entries`.

For each `p` that lies strictly inside an IR block (`b.start < p < b.end`, `p < code.len()`) and is not
already a compiled block start, the pass lowers `[p, sub_block_end(code, b, p))`, where `sub_block_end`
caps the containing IR block's `end` at the **next continuation address** in the same function
(`(start..code.len()).find(stores_continuation).map(|pc| pc + 1)`), or the block's `end` when no later call
exists. The emitted sub-block ranges are therefore **disjoint**, so the total lowered length is
**linear** — O(`code.len()`) — rather than the quadratic overlapping `[p, block.end)` tails for n calls in
one straight-line block.

The truncation loses no native coverage: the last instruction of `[p_i, p_{i+1})` is the frame-preserving
call at `p_{i+1} - 1`, which always transfers control out of the block (a deopt / `ENTER`, or a `call_bif`
continue that the shared `done` epilogue turns into an `encode_target(func, p_{i+1})` word), and
`p_{i+1}` has its own sub-block entry — so the driver resumes natively at the next sub-block. An address
already at a block start, or at/past the end of the function, needs no sub-block.

Sub-blocks are emitted after all IR blocks, so the block-entry table and the `code_map` regions keep
describing IR blocks only; they reserve no entry label (nothing branches to a continuation address), they
start with a full block prologue, and they are not counted as blocks. `entries` is probed **first** and
`sub_entries` second, so a real block start always wins. The lazy pass folds its register touches into the
per-function `Summary` as a superset; the eager pass reuses the **containing IR block's** allocation and
discards its touches, so the whole-module `Summary` is provably untouched. Both add
`sub_blocks_capacity_estimate` (`BLOCK_BYTES + INSTRUCTION_BYTES` per instruction of each sub-block range)
to the buffer size, so the extra code cannot under-allocate the `CodeBuffer`.

Real-module figures are unchanged where a block holds one continuation per tail (the cap is inert), and a
call-dense synthetic body costs ~4× per 4× call count (linear), pinned by
`emit/compile.rs::the_continuation_sub_blocks_scale_linearly_not_quadratically` (both layouts) and
`::the_disjoint_sub_blocks_still_run_every_call_ext` (all calls executed on both tiers).

## Register cache: partial spill AND partial fill (liveness-based)

`regs.rs` owns the block register cache: a fixed per-block set of x-registers
(`Allocation::slots`) is held in machine registers for the block's whole duration, so an
x-register is either *always* cached or *never* cached and the two representations can
never disagree. Both directions of the traffic are **partial**:

- `spill(buffer, allocation, SpillSite)` stores exactly `Allocation::written` — the
  block's statically-derived write set (`regalloc.rs::cacheable_writes`, a superset of
  what the inline lowerings actually write). A cached register outside that set still
  holds its `machine.x` value at every program point, so storing it is a no-op. Sites:
  `SpillSite::Helper` (before an in-block helper call — the helper *is* the interpreter
  and reads `machine.x`) and `SpillSite::Exit` (the block exit and the tail exit).
  Because `resident ⊆ slots` and a non-resident slot's `machine.x` is already
  authoritative, storing the write set is exact on every path — including the deopt and
  epilogue paths.
- `fill(buffer, allocation, FillSite, pc)` reloads exactly `Allocation::fill_slots(pc)`:
  the cached registers the continuation at `pc + 1` can still observe, computed
  block-locally and conservatively by `liveness.rs::fill_sets` as
  `(live ∪ written) ∩ slots` where `live = (live \ cacheable_writes) ∪ cacheable_touches`
  in a backwards walk over the block. The write-set term is the **join rule** for a
  guarded lowering's fallback label: on the fast path the destination register is
  authoritative and `machine.x` stale, on the fallback path the reverse — reloading the
  write set makes them agree, so the exit spill (which stores the write set on both
  paths) stays exact. A cached register left out of the fill set keeps a stale word,
  which is safe: a later **read** through its machine register makes it live at that
  helper (so it is reloaded) and every **store** stores exactly the write set, which the
  fill set always contains. The register is only ever overwritten, never read.

`Allocation` carries `block_start` and `fill_sets` (indexed by `pc - block_start`),
derived once by `regalloc.rs` at allocation time so the emitter stays a pure reader. An
**empty** `fill_sets` table (a synthetic `Allocation` a unit test built directly) means
"no liveness was computed" and makes every fill set the whole cache. The prologue load is always the whole cache (`FillSite::Prologue`),
and the table is always block-length so an in-block pc always resolves. This keeps
`allocate_registers(module, code, start, end) -> Allocation` and
`Allocation::slots`' meaning unchanged, so `compile.rs` (and the continuation
sub-blocks, which drive `emit_block` with their own `start`) compile unchanged.

Helper sites and their treatment: `jit_step_one` (the interpreter — may write any
x-register), `jit_ext_call`, the comparison-`test` helper `jit_test` (it can fall back
to `jit_step_one`), the receive/send family (`emit_receive`), the family arm (`jit_family`)
and the local `Call` store (`jit_call_store`) all route through
`emit_helper_call` + `emit_continue_check` (or `test.rs::emit_test_helper`'s own
`success` arm) and are therefore covered by `fill_slots`. The frame-discipline trio
(`frame.rs`: `deallocate`/`return`/`deallocate_return`) and the frame-only
heap-admission pair (`heap.rs`: `allocate`/`trim`) use helpers that provably write no
x-register, so those paths deliberately emit **no** refill. `counters.rs` yields both
models from one run: `fill_registers` (partial) and `fill_registers_full` (`Σ slots.len()`,
the whole-cache count), the latter attributed per site by a `PENDING_FILLS` queue that
`record_block` drains.
