# `emit/shapes` — the lowering dispatch, the restricted shapes, and the deopt levels

## Intent
The compile-time inlinable-vs-deopt decision: `block.rs::emit_block`'s per-instruction dispatch, the per-group shape predicates that decide it, the three deopt levels, and the heap / frame-admission family's lowering contract.
Whole-instruction inlining is a **compile-time, syntactic** decision (never a value test); every other shape is the `_` arm's per-instruction deopt to `jit_step_one`.

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
| Family-owned surface | `PutList`,`PutTuple2`, the native-record set, `MakeFun3`,`CallFun`,`CallFun2`,`Apply`,`ApplyLast`, the try/catch/raise set, the float set, the map set, `RecvMarker*` | `family.rs::emit_family` (an `emit_helper_call` to `jit_family`) | n/a — the helper runs the owning family's own body and reports the block-ABI word |
| Bitstring family | exactly the variants `families::bitstrings::claimed` names (20: `BsStartMatch3/4`, `BsGetPosition`, `BsSetPosition`, `BsGetTail`, `BsTestTail2`, `BsGetInteger2`, `BsGetBinary2`, `BsSkipBits2`, `BsMatch`, `BsMatchString`, `BsGetFloat2`, `BsGetUtf8/16/32`, `BsSkipUtf8/16/32`, `BsInitWritable`, `BsCreateBin`) | `family.rs::emit_bitstr` (the **guard** arm `_ if bitstrings::claimed(instr)`) | n/a — the same `emit_step_helper` sequence as `emit_family`, plus the inline position store (`emit_positioned_helper_call`) because `jit_bitstr` stores no position itself |
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
  **It is not a definition of its own: it *delegates* to `crate::interpreter::constant_word`, the single
  authority on "decidable by a raw-word compare" shared with the interpreter's `select_val` ladder (and, through
  `jit/cranelift/inline.rs`, with the Cranelift tier).** So the alternatives this tier emits a word compare for
  are exactly the alternatives the interpreted ladder decides without a general comparison, and the two cannot
  drift.
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
  alternative must be an immediate `constant_word` (i.e. the shared
  `crate::interpreter::constant_word` answers it, so the set of alternatives inlined here is exactly the set the
  interpreted ladder decides by word identity). `SelectPlan::uses_y` gates the Y publish/refresh;
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
  cached-register clobber). Pinned by
  `tests/jit.rs::tuple_writes_match_the_interpreter_on_real_tuples`,
  `::the_tuple_write_is_inlined_and_caches_its_registers`, and the two rejected-target tests
  (`a_non_tuple_tuple_write_target_…`, `a_literal_area_tuple_write_target_…`).
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

## See Also
- `../../CONTEXT.md` → the tier contract, the two layouts, the driver, the routing table.
- `../CONTEXT.md` → the emitter contract (core model, spill/fill pointer, routing, constraints).
- `../prologue/CONTEXT.md` → the block prologue and the Y-window publication the guarded shapes depend on.
- `../link/CONTEXT.md` → the exits, the local-call transfer and the continuation sub-blocks.
- `../../testing/CONTEXT.md` → the suites that pin each shape's agreement and its no-deopt claim.
