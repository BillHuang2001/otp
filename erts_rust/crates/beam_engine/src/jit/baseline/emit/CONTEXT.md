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
`jit_set_position`, `jit_publish_y`, `jit_refresh_y`) live one level up in `../mod.rs`,
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
  `FAST_A`=`rax`, `FAST_B`=`r8`, `FAST_C`=`rcx`, `FAST_T`=`r10`, `FAST_R`=`r11` (`arith.rs`);
  `TEST_A`=`rax`, `TEST_B`=`r8`, `TEST_C`=`r9`, `TEST_T`=`r10`, `TEST_H`=`r11` (`test.rs`);
  `GUARD_SCRATCH`=`r8` (`aggregate.rs`); `Y_ADDR_SCRATCH`=`r9` / `Y_ADDR_SCRATCH_2`=`r8`
  (`operands.rs`, the pre-computed Y destination addresses for a multi-destination writer).
  None overlaps a cache register, `rbx` or `rsp`.

## The lowering dispatch (the compile-time inlinable-vs-deopt decision)
`block.rs::emit_block` matches each `Instr` of the block (the `match instr { .. }`).
Each arm calls the group's `try_inline_*`; **`Ok(false)` means "lowered nothing"** and the arm then
emits `emit_deopt`.
Whole-instruction inlining therefore is a **compile-time, syntactic** decision (never a value test).
Logical groups:

| Group | Instr(s) | Lowering entry | Shape/guard predicate |
|---|---|---|---|
| Metadata no-ops | `Label`,`Line`,`IntCodeEnd`,`ExecutableLine`,`DebugLine` | `block.rs` empty arm (no bytes) | n/a |
| Move / swap | `Move`, `Swap` | `r#move.rs::try_inline_move`, `::try_inline_swap` | `r#move.rs::move_shape`: source = in-bound `X(n)`, in-bound **`Y` slot**, a constant word or a **literal-area** term (`MoveValue::Literal`); destination = in-bound `X(n)` or an in-bound **`Y` slot** (a `Y` side and a literal source each carry a run-time guard). `swap_shape` accepts two `Loc` operands and lowers a `Y` side via `emit_swap_y` |
| Y initialization | `InitYregs` | `block.rs::try_inline_init_yregs` | `operands.rs::locate_all` — every listed operand an in-bound `X(n)` or an addressable `Y` slot |
| Test (word + boxed header) | `Test` | `test.rs::try_inline_test` | `test.rs::test_plan` (a `TestSrc::Y` operand carries a run-time guard) |
| Switch | `SelectVal` | `select.rs::try_inline_select_val` | `select.rs::select_val_plan` (a `TestSrc::Y` scrutinee carries a run-time guard) |
| Arity switch | `SelectTupleArity` | `aggregate.rs::try_inline_select_tuple_arity` | `aggregate.rs::select_tuple_arity_plan` (the tuple operand is a `Loc`) |
| Cons readers | `GetList`,`GetHd`,`GetTl` | `aggregate/list.rs::try_inline_get_{list,hd,tl}` via `emit_list_read` | `aggregate/list.rs::get_{list,hd,tl}_shape` -> `shape`; source and every destination are a `Loc` |
| Tuple read | `GetTupleElement` | `aggregate.rs::try_inline_get_tuple_element` via `emit_tuple_read` | `get_tuple_element_shape` (source and destination are a `Loc`) |
| Tuple write | `SetTupleElement` | `aggregate.rs::try_inline_set_tuple_element` via `emit_tuple_write` | `set_tuple_element_shape` (the tuple is a `Loc`) |
| Arithmetic gc_bif | `GcBif` (`+`/`-`/`*`) | `arith.rs::try_inline_gc_bif` | `arith.rs::fast_arith` (x-register **or Y-slot** operand/destination; a Y access carries a run-time frame-window guard). `band`/`bor`/`bsr` are lowered by the same entry (`GcBif` only — the corpus never emits them as `Bif`) |
| Comparisons | `Bif` (and `GcBif` fallback) | `compare.rs::try_inline_comparison` | `compare.rs::comparison_shape` |
| BIF accessors | `Bif` (`hd`, `element`, `tuple_size`) | `bif.rs::try_inline_bif` -> the reused `aggregate.rs` cons/tuple read | `bif.rs::bif_shape` (the cons/tuple read shapes; `element/2` additionally needs a **compile-time small-int index `>= 1`**) |
| BIF value accessors | `Bif` and `GcBif` (`byte_size`, `length`) | `bif.rs::try_inline_bif` -> `emit_helper_call(jit_bif_value)` (`../bif_helper.rs`) | `bif.rs::value_shape` (the operand is re-read and written through the engine's own `read_operand`/`write_operand`) |
| Frame pop | `Deallocate` | `frame.rs::emit_deallocate` (an `emit_helper_call` to `jit_deallocate`) | always lowered (the instruction's own outcome is the helper's block-ABI word): continues in-block on `encode_target(func, pc+1)` via `emit_frame_change_continue` (no refill; the helper re-published the Y window), else leaves via `done` |
| Return | `Return` | `frame.rs::emit_return` (an `emit_helper_call` to `jit_return`) | always lowered — a `return` always transfers control: `emit_resume_or_exit` tail-jumps to the continuation entry the helper resolved (in this function, or in another function of the same module), else the block leaves with the helper's word in `rax` |
| Fused frame pop + return | `DeallocateReturn` | `frame.rs::emit_deallocate_return` (an `emit_helper_call` to `jit_deallocate_return`); `emit_block` also fuses a separate adjacent `Deallocate`+`Return` pair into one such call | always lowered — always transfers control, leaves via `emit_resume_or_exit` (in-block tail-jump, else the helper's word) |
| Frame push | `Allocate` | `heap.rs::emit_allocate` (an `emit_helper_call` to `jit_allocate`) | always lowered (the helper's `Machine::push_frame`; `allocate` performs no heap admission): continues in-block on `encode_target(func, pc+1)`, else leaves via `done` |
| Frame push + heap admission | `AllocateHeap` | `heap.rs::emit_allocate_heap` (an `emit_helper_call` to `jit_allocate_heap`) | always lowered — one helper round-trip does the interpreter's two calls in order (`ensure_heap` **then** `push_frame`); continues in-block via `emit_continue_check_helper_refresh` (partial refill, **no** emitted `jit_refresh_y` — the helper re-publishes the window itself) |
| Heap admission | `TestHeap` | `heap.rs::emit_test_heap` (an `emit_helper_call` to `jit_test_heap`) | always lowered — reaches the host's admission door exactly once with the instruction's own `heap_need`/`live`; continues in-block via `emit_continue_check` (partial refill + the gated Y refresh — this helper changes no frame) |
| Frame trim | `Trim` | `heap.rs::emit_trim` (an `emit_helper_call` to `jit_trim`) | always lowered — an over-trim / frameless trim is the helper's `ERROR`; continues in-block on `encode_target(func, pc+1)` via `emit_frame_change_continue` (no refill; the helper re-published the Y window) |
| Jump | `Jump` | `link.rs::Linker::emit_exit` | resolved label **in the block map** -> linked; otherwise return-to-driver (the target's function is compiled on entry) |
| Local tail call | `CallLast` | `block.rs::emit_local_call_probe` then `link.rs::Linker::emit_tail_exit` (compiled successor) or `::emit_tail_exit_to_driver` (resolved but uncompiled successor) | `Linker::is_compiled`; an undefined label keeps the `emit_deopt` `unknown_label`. The probe carries the resolved callee/arity (packed), the `LOCAL_CALL_LAST` kind and the `deallocate` size as block-ABI immediates, and both exits pass `ExitSpill::Done` (the probe already spilled at the same `pc`) |
| Local tail call (no pop) | `CallOnly` | `block.rs::emit_local_call_probe` then `link.rs::Linker::emit_exit` | resolved label; the same baked immediates with the `LOCAL_CALL_ONLY` kind, and the exit passes `ExitSpill::Done` |
| Local frame-preserving call | `Call` | `block.rs`: `emit_helper_call(jit_call_store)` + a `CALL_STORED` compare, then `link.rs::Linker::emit_exit` when `is_compiled` holds the callee's block, else `emit_helper_call(jit_call_store_linked)` + `link.rs::emit_in_block_call` | resolved label; the helper stores the continuation *and* consults the local-call seam, and the linking twin also resolves the callee's native entry so the block tail-jumps into it (declines to the driver's word); the linked exit passes `ExitSpill::Done` (the same helper call already spilled at this `pc`) |
| Cross-module call | `CallExt`,`CallExtOnly`,`CallExtLast` | `block.rs::emit_ext_call` (an `emit_helper_call` to `jit_ext_call`) | n/a — the helper resolves the import and reports `ENTER` |
| Receive/send control | `Send`,`LoopRec`,`LoopRecEnd`,`Wait`,`WaitTimeout`,`Timeout`,`RemoveMessage` | `block.rs::emit_receive` (an `emit_helper_call` to `jit_receive`) | n/a — the helper runs the family body and reports the block-ABI word |
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
Note `GcBif` tries arith **then** comparison **then** the BIF accessors; `Bif` tries the comparison
shape then the BIF accessors. The settled corpus emits the eight comparison ops and `hd`/`element`/
`tuple_size` as non-collecting `Bif`, and `byte_size`/`length` as collecting `GcBif` (so the value
accessors are claimed from either variant and the three pure accessors from `Bif` only).

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
- **Cons/tuple readers** (`aggregate/list.rs` for the cons readers `get_list`/`get_hd`/`get_tl`, `aggregate.rs` for the tuple readers): the source and every destination must be a `Loc` (an
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
  `band`/`bor`/`bsr` join the same predicate (`SmallArith::{And,Or,Bsr}`), reached from the `GcBif` arm
  only (the corpus never emits them as `Bif`): `band`/`bor` emit a raw **tagged-word** `and_rr`/`or_rr`
  — exact even for negative smalls, because both operands' low nibble is the small tag, so there is no
  untag/retag and no range guard — while `bsr` emits untag → `sar_ri` → small-range check → retag and
  claims only an **immediate small-int constant count in `0..64`** (a register count has no immediate
  encoding, a negative count is a `bsl` that may overflow to a bignum, and `>= 64` would be masked by
  the hardware, so all three deopt). A bignum/float/other operand on any of the three deopts.
- **compare** (`compare.rs::comparison_shape`): only the eight `erlang` comparison ops at arity 2; both
  operands must be `cmp_operand`; a derivable guard; and the module must have both `true` and `false`
  interned (else the whole instruction deopts — a wrong atom is never fabricated). `Fail` is
  deliberately not inspected (the fast path never errors; a guard rejection deopts to the full step).
- **bif** (`bif.rs::bif_shape` / `value_shape`): only an `erlang` import whose **name** is one of the
  five lowered accessors and whose arity matches the instruction's, and only an operand shape the
  lowering can prove. `bif_shape`/`bif_uses_y` are **purely syntactic** (import names + operand kinds,
  never the register allocation), so `regalloc.rs` and `emit_block` agree. `hd/1` needs a `ListShape`
  source (`Loc`); `element/2` needs a `TupleReadShape` tuple **and a compile-time small-int index
  `>= 1`** (`element_index` — a non-immediate index keeps the interpreter's `badarg`); `tuple_size/1`
  needs a `TupleReadShape` source/destination (its element offset is never read); `byte_size/1`/
  `length/1` (`value_shape`) need a single argument that `bif_helper.rs::read_is_allocation_free`
  accepts — only a **non-small `Integer` constant** would make `read_operand` materialize a bignum, and
  such a shape is not a fast path. The three pure accessors are claimed from `Instr::Bif` only;
  the two value accessors from **either** `Bif` or `GcBif` (the corpus's `byte_size`/`length` are
  collecting), and the helper deliberately reproduces no fail label or raise of its own — a shape it
  cannot serve hands the **whole** instruction to `jit_step_one`. Every rejection branches to the same
  `emit_step_and_continue`: a non-addressable operand, a non-immediate/`0` index, a non-cons or
  non-tuple source, an out-of-range element, a non-bitstring, an improper list, an out-of-small-range
  count. The fast path **allocates nothing and writes no heap word**.

## Deopt mechanism (three levels)
1. **Compile-time shape refusal**: a `try_inline_*` returns `Ok(false)` **without emitting a byte**, and
   the dispatch arm emits `block.rs::emit_deopt` in its place; the `_` arm does the same for any unclaimed
   `Instr`. `emit_deopt` records the static-census deopt, then runs `emit_step_and_continue`.
2. **Run-time guard rejection**: an inlined sequence that passed the shape test but whose *runtime* guard
   fails branches to a local `fallback` label that runs `emit_step_and_continue` and then falls through
   to the rest of the block (`fast_done`). Present in `arith.rs`, `compare.rs`, `bif.rs`, `test.rs`,
   `aggregate/list.rs` (`emit_list_read`), `aggregate.rs` (`emit_tuple_read`/`try_inline_select_tuple_arity`), `select.rs`, and
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
   `emit_return`/`emit_deallocate_return` always transfer control and leave via `emit_resume_or_exit`;
   `heap.rs::emit_allocate_heap`/`emit_test_heap`, `emit_ext_call`, `emit_receive` and `emit_family`
   continue through the shared refill arm (`emit_continue_check`, or `emit_continue_check_helper_refresh`
   for the two frame changes, which re-publish the Y window inside the helper) — a collecting host's
   admission rewrites `machine.x`, so the cache is refilled before the block resumes. There are no guards to emit: every
   operand shape is lowered (a frame size / heap need / live count / trim `n` is a `u32` the helper
   takes as an argument, and a `return` has no operand), and the only failure is a `CodeBuffer` that
   cannot hold the bytes — the shared typed `SystemLimit`.

`emit_step_and_continue` (`mod.rs`) = one `jit_step_one` call + `emit_continue_check`: `rax` is compared
against `encode_target(func, pc+1)`; equal -> `fill` and fall through natively at `pc+1`; any other word
(`Goto`/`HALT`/`ERROR`/`ENTER`/`Wait`) -> `jmp done`, the shared epilogue, which `ret`s to the driver with
the word in `rax`. So a non-`Next` helper outcome and the return-to-driver deopt share one epilogue.
`emit_ext_call` (`block.rs`, the `CallExt*` arm) is the **second** caller of that same check: it emits an
`emit_helper_call` to `jit_ext_call` for the instruction itself — import resolution plus the variant's
frame discipline, no interpreter step — and then `emit_continue_check`. Its `Next` case is the helper's
`jit_step_one` fallback (the synchronous `call_bif` path); its other word is the `ENTER` of a resolved
cross-module transfer, which the driver decodes into the context switch.
`link.rs::emit_exit`'s unlinkable path is a **different** mechanism: it is *not* a helper call — it spills,
materialises the return word in `rax`, and jumps to `done`; the **driver** then looks the target up in the
active artifact — for a lazy artifact a branch to another function is simply absent from that function's
block map, so the driver compiles the target's function on entry — or takes one interpreter step.
The frame-preserving `call` is the one shape that **no longer takes that path** for a cross-function callee:
`link.rs::emit_in_block_call` is its `emit_resume_or_exit`-shaped twin (`mov rdi,[rsp+RUNTIME_SLOT_DISP]` /
`mov r11,[rdi+RUNTIME_RESUME_DISP]` / `test r11,r11` / `je declined` / `emit_frame_teardown` / `jmp r11`),
emitted after `jit_call_store_linked` has already published the callee's entry plus the skipped driver
iteration (position store, reduction bump, `ctx.reduce(1)`). It emits **no** second spill (the helper call
spilled the dirty set already) and the transfer is a **tail** transfer, so Erlang non-tail recursion does not
grow the native stack. A `CallLast` into a resolved-but-uncompiled successor and every other unlinkable jump
still take `emit_tail_exit_to_driver`/`emit_exit`.
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
  registers still hold exactly what `machine.x` does); because they *are* the frame change they
  re-publish the Y window **inside** the helper, so the continue arm is the private
  `emit_frame_change_continue` (the shared compare-and-branch **without** a refill **and** without an
  emitted `jit_refresh_y`) — it needs no `Allocation`, there is no gate left to consult.
- **heap-admitting** — `allocate_heap`/`test_heap` (`emit_allocate_heap`/`emit_test_heap`) reach the
  host's admission door, and a **collecting** host relocates the young heap and rewrites every root **in
  place** (including `machine.x`), so the block's cached x-registers are stale the instant the helper
  returns: both continue through the partial refill, which *is* the post-admission contract —
  `emit_continue_check_helper_refresh` for `allocate_heap` (no emitted `jit_refresh_y`, the helper
  re-published the window) and `emit_continue_check` for `test_heap` (which changes no frame, so its
  gated Y refresh survives). `allocate_heap`'s single round-trip does the interpreter's two calls in
  order (`ensure_heap` **then** `push_frame`).

**There are no guards to emit** — every operand shape is lowered (each of `stack_need`/`heap_need`/`live`
is a `u32` the helper takes as an argument, computed at compile time by `HeapNeed::words()`, and `trim`'s
`n` likewise) — so the only failure is a `CodeBuffer` that cannot hold the emitted bytes, the shared typed
`SystemLimit`. The two heap-admitting lowerings reach the host's admission door **exactly once** with the
instruction's own operands.

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
  counters (grand total over the generically-discovered installed-OTP corpus): 597 333 blocks /
  756 632 cache slots, push/pop ops 7 167 996 → 2 707 930 (−62.2 %), emitted bytes
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
- `link.rs::emit_exit`, `link.rs::emit_tail_exit` and `link.rs::emit_tail_exit_to_driver` spill (the
  dirty set at their own `store_pc`) on **every** path — the linked and the return-to-driver ones alike —
  except where the caller passes `link.rs::ExitSpill::Done` (see the next bullet).
  `link.rs::emit_in_block_call` deliberately emits **no** spill of its own: it only ever runs after the
  `jit_call_store_linked` `emit_helper_call`, which already spilled the dirty set, and both its arms then
  leave `machine.x` authoritative.
- **`link.rs::ExitSpill::{Required, Done}` — an exit skips only a *duplicate* store of the same dirty
  set.** A `spill` is site- and path-independent: it stores exactly
  `Allocation::spill_slots_at(pc)` at the `pc` it is given, whatever `SpillSite` asked for it. So a
  mandatory `SpillSite::Helper` store at an instruction's own `pc` (from `emit_helper_call`) and a
  block exit's `SpillSite::Exit` store at the **same** `pc` would write the identical words twice. The
  mode every exit emitter takes — `ExitSpill::emit`, the one place the exit store is made:
  - `Required` — the exit owns the store, because nothing spilled ahead of it: the `Jump`, the
    `test`/`select_val`/`select_tuple_arity` fail-label branches, and the block's fall-through;
  - `Done` — the exit emits nothing, and is passed **only** where an `emit_helper_call` at the same
    `pc` immediately precedes the exit: the three probed local-call shapes (`CallLast`'s
    `emit_tail_exit`/`emit_tail_exit_to_driver`, `CallOnly`'s `emit_exit`, and the frame-preserving
    `Call`'s linked `emit_exit`), whose helper is `jit_local_call`/`jit_call_store*`.

  `Done` is sound because nothing emitted between that helper call and the exit writes a cached
  x-register (a helper `call`, a `mov` of a constant into the caller-saved scratch `r11`, a `cmp`, a
  `jcc`) and both helpers' **decline** paths mutate nothing, so the words already stored are still the
  ones `machine.x` must receive on every path the exit can run — it removes stores only, never a
  store the helper site owes (the helper spill is the tier's root-set contract, so it is never the one
  removed), and the fill model is untouched.
  Corpus figures (`../census.rs::the_register_cache_traffic_census_over_the_corpus_is_printed`,
  deterministic, no threshold: 1358 modules / 103 066 functions / 597 507 blocks): exit-site spill
  calls **797 396 → 705 569** (−91 827, −11.5 %), total spill calls 2 603 645 → 2 511 818;
  dirty-set spill stores **1 496 158 → 1 359 648** (−136 510, −9.12 %), of which exit-site stores
  415 661 → 279 151 (−32.8 %); every fill figure is unchanged (liveness 3 291 217 on both sides), and
  the emitted bytes of the corpus move **316 324 782 → 315 369 212** (−955 570, −0.30 %) on the tree
  that already carries the baked tail-call immediates (those immediates themselves cost +2 432 640
  bytes, +0.78 %, on the un-probed tree, so the net over both mechanisms is +1 477 070 / +0.47 %). The
  single-store saving is −7 bytes per probed exit, i.e. one `mov [rbx+disp], reg`.
- The epilogue (`block.rs`: `done` -> `emit_frame_teardown` -> `ret`) does **not** spill: every path reaching
  it has already spilled. The inline fast paths (`move`/`arith`/`compare`/`test`/`select`/`aggregate`) do
  **not** spill themselves — a result lands in a cache register or straight in `machine.x` (the very slot
  the exit spill writes), and the next helper call or block exit stores it (a result left in a cache
  register is dirty at that point, so the store set contains it).

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

## Local calls — the frame-preserving `Call`, `CALL_STORED`, and the tail forms' baked probe
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
The arm resolves the callee **at compile time** and hands the helper the answer: `block.rs` turns the
target label into its `(function index, block pc, arity)` through the same map the `Linker` uses for the
branch, so `jit_call_store` receives a `CallSite` and never re-derives the callee's identity from the
instruction word. Emitter and helper are pinned to each other by a `debug_assert!` on that agreement,
which is the invariant to preserve if either side's operand handling changes: a mismatched pair would
store a return address into the wrong function's frame and only show up as a corrupted continuation.

### The transfer: two shapes, chosen at compile time
`Linker::is_compiled(target_func, target_pc)` decides how the block leaves after `CALL_STORED`:
- **the callee's block is in this compilation** (the eager whole-module artifact, or a call inside the
  same function) — `link.rs::Linker::emit_exit`'s linked arm, a direct `jmp` to the target block, exactly
  as `call_only` does;
- **it is another function the lazy per-function artifact has not compiled** — the shape every
  cross-function non-tail call takes on the runtime path (`CodeCache::compiled_handle`) — the arm asks for
  `call.rs::jit_call_store_linked` instead, and leaves through `link.rs::emit_in_block_call`. That helper
  stores the continuation and runs the seam **first** (a served callee must get neither a return address
  nor a transfer), then resolves the callee's native entry through `runtime.rs::enter_in_block`, which
  lazily compiles the function on first entry, checks its register-file bound, publishes the target's
  `EntryTables` and performs the skipped driver iteration in the driver's own order (position store,
  reductions bump, `ctx.reduce(1)?`). Every decline leaves `resume_entry` null and the helper's word equal
  to the pre-existing `encode_target(func, pc)`, so the driver serves it exactly as before.
Pinned by `call_tests.rs`'s five transfer tests (see `../CONTEXT.md`).

### The tail forms (`CallLast`/`CallOnly`): the same compile-time-baked probe
The two tail forms are probed through `block.rs::emit_local_call_probe` →
`../runtime.rs::jit_local_call`, and they receive the same treatment as the frame-preserving `Call`:
the emitter's **whole compile-time resolution arrives as block-ABI immediates** — the caller's
`(func, pc)`, `runtime.rs::encode_local_call(callee, arity)` (the packed `(function index, arity)`
word, so the five payload words still fit `emit_helper_call`'s argument registers), the variant kind
(`LOCAL_CALL_LAST`/`LOCAL_CALL_ONLY`) and, for a `call_last`, its `deallocate` frame size — exactly as
`jit_ext_call` receives its `ImportRef`/kind/size.
`runtime.rs::probe_local_call` therefore re-derives nothing on the per-call path: it does **not**
re-read `module.functions[func].code[pc]` and does **not** re-resolve the target label (the `Linker`
already resolved it for the block's own exit), it just decodes the pair, builds the arm's
`ImportFrame` from the kind and consults `Machine::local_call_seam`.
The instruction re-derivation is retained **only** as a `debug_assert!` cross-check — the address
names this very tail call, its `arity` operand is the one handed to the seam, the baked kind names
the variant (`call_last`'s `deallocate` too) and `labels.resolve(target)` equals the baked callee —
so an emitter/helper divergence stays a test-time failure at zero release cost (the label map is
dropped from the release body, `#[cfg(not(debug_assertions))] let _ = labels;`). The emitter is the
single source of truth, which is also what keeps the immediates honest: it resolved the label for
the exit it emits anyway.

Cost, per kind (`../census.rs::the_per_kind_emitted_code_size_table_is_printed`): `CallLast
(unlinked exit)` measures **200 → 230** whole-block bytes — the +30 nets the three extra helper
arguments it now passes (the packed callee/arity word, the kind, the frame size: +37 bytes) against
the −7-byte single-store saving the exit's `ExitSpill::Done` mode gives back (see
`## Spill/fill discipline`); the corpus-wide totals are there too.

## Measured

### The dead prologue Y-window publish (measured)
`emit/prologue.rs` emits `jit_publish_y` only when `Allocation::prologue_publishes_y` holds, so a block whose
**every** `Y` access follows an in-block `allocate`/`allocate_heap`/`trim` (`emit/regalloc.rs`'s
`y_first` vs `frame_republish_first`) emits the plain `jit_set_position` instead — the frame helper's own
`JitRuntime::refresh_y_base` on its in-block continue arm makes the prologue's publish dead work (soundness:
`## Spill/fill discipline` above; the full rule and its linear-control-flow argument in `../CONTEXT.md`).
Two retired-user-instruction slopes, per-rep, two legs from the same tree with only this gate differing,
sides interleaved and the order alternated, `taskset -c 3`, one process at a time: `list_walk`
(`BENCH_TIER=baseline BENCH_K=20`, `i(4000) − i(500)`) **−2.33 %** (paired min / median **0.976676 /
0.976684**, 5 rounds, zero overlap) and `foreign_cowboy_bstr`
(`to_lower/1` 438 + `capitalize_token/1` 660 IR instructions, `i(101 000) − i(1 000)`) **−0.40 % / −0.36 %**
(**0.996005 / 0.996398**) — small because the entry-side saving is diluted by the 1 098-instruction call
body. About half of a **test** build's saving is the `#[cfg(test)]` counter probe in `jit_publish_y`, so the
counter-borne figure overstates a production build; the sign and the mechanism do not depend on it. The
per-workload rows, the round-by-round ratios and the corpus-wide `y_blocks` / `y_prologue_publishes` split are
in `../CONTEXT.md`'s `## Measured`.

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
  `emit_step_and_continue`, `emit_continue_check`, `emit_continue_check_helper_refresh`; re-exports
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
  live-in cache load) plus the private `emit_position_store`/`emit_publish_y`/`emit_prologue_helper`
  it uses.
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
- `regalloc.rs` -> `allocate_registers` (incl. the `y_last`/`y_first`/`frame_republish_first`, `written`
  and `spill_sets` derivations) +
  `cacheable_touches`
  (the per-instruction native touches) + `cacheable_writes` (the block's written x-registers, mirroring each lowering's shape
  test but returning destinations) + `spill_sets`/`is_inlined`/`test_helper_inlined` (the per-pc
  dirty-set walk and its inlining mirror).
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
  frame-admission family — a direct `emit_helper_call` to the `../runtime.rs` helpers) and the private
  `emit_frame_change_continue` (the frame-only helpers' continue arm: compare against
  `encode_target(func, pc+1)`, no register refill and no emitted refresh — the helper re-published the
  Y window), plus its unit tests (the spill-first call order, the frame-only-vs-heap-admitting refill
  split, and `a_frame_changing_helper_emits_no_separate_refresh_call`).
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
- `baseline/tests.rs` glob-imports `emit`; `mod.rs` re-exports some items under `#[cfg(test)]` and
  `pub(crate) use block::emit_block;`/`pub(super) use list::{…};` re-exports keep every `emit::X` path
  stable across the splits. Keep that in mind when renaming items.

## Known Issues
- **`block.rs` holds the single per-instruction lowering dispatch** (`emit_block`, a `match instr` with
  one arm per instruction group) plus its deopt wrapper and the three helper-sequence arms; the `match`
  is what makes the file cohesive — each arm is one `try_inline_*` call plus an `emit_deopt` fallback —
  so splitting it further would scatter one decision across files or require inventing a visitor/table
  abstraction the emitter does not need. The block prologue it emits first is `prologue.rs`; the other
  files are under the ~1000-line guidance (`test.rs` 963, `compile.rs` 892, `mod.rs` 401, `aggregate.rs`
  744, …).
- **`test.rs` (963) and `aggregate.rs` (744) are under the guidance but still large**: `test.rs` keeps
  the plan machinery (`test_plan`/`TestPlan`/the mask constants) because both `try_inline_test` and the
  `test/header.rs` emitters read them; `aggregate.rs` keeps the tuple path while the cons-cell readers
  live in `aggregate/list.rs`.

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
`sub_entries` second, so a real block start always wins. **Each pass allocates for the range it emits** —
`emit_sub_blocks` folds its sub-block touches into the per-function `Summary` as a superset while
`emit_shared_sub_blocks` derives the same `allocate_registers(module, code, start, end)` and discards its
touches, so the eager whole-module `Summary` stays exactly the IR blocks'. A sub-block is a fresh native
function whose prologue loads its own live-in set, so its `Allocation` must be entry-point-relative to
`start`: the containing IR block's allocation is indexed from `block.start` (`block_start`, the per-`pc`
`fill_sets`/`spill_sets` tables, the live-in `entry_fill` set — see `regs.rs`), and a cached register the
block first *writes* before `start` would not be loaded, while the dirty bits accumulated at pcs `< start`
would make the sub-block's first spill store a leftover machine register into `machine.x`. Both add
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

- `spill(buffer, allocation, SpillSite, pc)` stores exactly `Allocation::spill_slots_at(pc)` —
  the cached x-registers **dirty at `pc`**: those an inline lowering has diverged from their
  `machine.x` slot since the last syncing event (the prologue, which loads the cache, or a
  post-helper `fill`, which reloads it). The set is a per-`pc` over-approximation derived by
  `regalloc.rs`'s forward `spill_sets` dirty walk (union `cacheable_writes` for an inlined
  instruction, clear it where the emission is entirely a helper, biased toward "inlined" so a
  missed reset only costs a redundant store). An allocation whose table is empty (a synthetic one
  a unit test built directly) falls back to the whole-block `Allocation::written` set. Sites:
  `SpillSite::Helper` (before an in-block helper call — the helper *is* the interpreter
  and reads `machine.x`) and `SpillSite::Exit` (the block exit and the tail exit — `link.rs::ExitSpill`
  decides whether that exit still owes the store or a preceding helper call at the same `pc` already
  made it; see `## Spill/fill discipline`).
  Because `resident ⊆ slots` and a non-resident slot's `machine.x` is already
  authoritative, the dirty set is complete on every path — including the deopt and
  epilogue paths.
- `fill(buffer, allocation, FillSite, pc)` reloads exactly `Allocation::fill_slots(pc)`:
  the cached registers the continuation at `pc + 1` can still observe, computed
  block-locally and conservatively by `liveness.rs::fill_sets` as
  `(live ∪ written) ∩ slots` where `live = (live \ cacheable_writes) ∪ cacheable_touches`
  in a backwards walk over the block. The write-set term is the **join rule** for a
  guarded lowering's fallback label: on the fast path the destination register is
  authoritative and `machine.x` stale, on the fallback path the reverse — reloading the
  write set makes them agree, and the exit spill's dirty set (which the dirty walk unions
  the same write set into, on either path) stores it, so the two stay exact. A cached
  register left out of the fill set keeps a stale word,
  which is safe: a later **read** through its machine register makes it live at that
  helper (so it is reloaded) and every **store** stores exactly the dirty set, which the
  fill set always contains. The register is only ever overwritten, never read.

`Allocation` carries `block_start`, `fill_sets` (indexed by `pc - block_start`, one entry
per pc of the block), `spill_sets` (the same indexing, one entry per pc in
`[start, end]` — the block's own pcs **plus** the exit pc `end`) and `entry_fill` (the
block's live-in cached slots), all derived once by `regalloc.rs` at allocation time so the
emitter stays a pure reader. An
**empty** `fill_sets` table (a synthetic `Allocation` a unit test built directly) means
"no liveness was computed" and makes every fill set the whole cache, and an empty
`spill_sets` table likewise makes every store set the whole `written` set. The prologue load is `entry_fill` (`FillSite::Prologue`), whose absent table likewise falls back to the whole cache,
and the fill table is always block-length so an in-block pc always resolves. This keeps
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
