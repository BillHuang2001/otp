# `emit/link` — the exits, the local-call transfer, and the continuation sub-blocks

## Intent
Every way an emitted range transfers control: the block-to-block link, the return-to-driver word, the `ExitSpill` single-store rule and the epilogue; the frame-preserving `Call`'s `CALL_STORED` + in-block transfer; the tail forms' compile-time-baked probe; and the continuation sub-blocks both artifact layouts emit.
A `CallExt*` transfer stays a **driver round-trip by design** (separate executable mappings, buffer-local `rel32` fixups, a module baked into `JitRuntime`); the in-block transfer covers a same-module callee only, and it is a **tail** transfer, so Erlang non-tail recursion does not grow the native stack.

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


## See Also
- `../../CONTEXT.md` → the driver loop, the block ABI words, the reduction charge a transfer reproduces.
- `../../runtime/CONTEXT.md` → `enter_in_block`, `return_word`, `resume_in_block`, `EntryTables`.
- `../prologue/CONTEXT.md` → the prologue every entered range opens with (a sub-block included).
- `../regalloc/CONTEXT.md` → the per-range allocation a sub-block derives for itself.
