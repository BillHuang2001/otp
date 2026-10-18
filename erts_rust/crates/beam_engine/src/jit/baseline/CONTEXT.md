# `beam_engine` native baseline JIT tier (`src/jit/baseline/`)

## Intent
The first `JitBackend` the engine builds: it lowers basic blocks of a `beam_loader::Module`
into real `extern "C"` x86-64 functions and runs them through a driver loop, deopting any
instruction it does not inline to exactly one interpreter step. Correctness-first mechanism, not an
optimiser (DeepDive §F): every natively-executed result is interpreter-identical by construction.
Backend behind `Tier::BaselineJit`; `jit-beamasm` aliases this tier.

The artifact (`CompiledModule`) comes in **two layouts**:
- **Eager** (`emit::compile_module`): every function lowered up front into **one** sealed
  `CodeBuffer`, with a whole-module block-entry table, so the seal-time `Linker` links a taken
  branch to a successor block of **another** function as a direct jump. Produced by
  `JitBackend::compile` and `CodeCache::get_or_compile`; the layout whole-module introspection and
  cross-function direct links need.
- **Lazy** (`code::CompiledModule::lazy`): nothing lowered up front; each function is lowered into
  its **own** sealed `CodeBuffer` on that function's first entry, driven through
  `NativeCode::enter(rt, module, func, pc)` → `CompiledModule::native_entry`. Produced by
  `CodeCache::compiled_handle` (the path behind `run_baseline_cached*` and
  `cached_module_blocks`), so a fresh process pays only for the functions it enters. A
  cross-function branch is absent from that function's block map, so the `Linker` emits the
  target's `(func, pc)` word and the driver compiles the target on entry.

## API Surface
- `code.rs` — `CompiledModule` (caller-owned artifact in the two layouts; `entry`, `native_entry`
  [the compile-on-entry point], `block_count`, `cached_registers`, `highest_cached_register`,
  `highest_native_register`, `code_len`, `code_address_range`, `code_map`, plus the two per-artifact
  derived values computed **once at creation**: `fingerprint` [the module's content hash, so a
  `CodeCache` hit never re-hashes the IR] and `labels` [the module-wide label map a run reuses
  instead of rebuilding]), plus the lazy layout's unit `CompiledFunction` (`buffer`, `entries` [IR-block
  starts], `sub_entries` [continuation sub-blocks], `summary`), the eager layout's parallel
  `EagerLayout::sub_entries` table, and the `Summary` register/block aggregate. `entry`/`native_entry`
  probe `entries` first and `sub_entries` second, so a real IR-block start always wins.
- `mod.rs` — `BaselineJit` (zero-sized `JitBackend`); the driver
  `run_compiled`/`run_compiled_outcome`/`run_compiled_label_outcome`, the no-reset
  `run_compiled_resume_outcome`, and the shared `enter_machine`/`run_native`/`run_native_outcome`/
  `run_native_label_outcome`/`drive`; the `RunLabels` enum (`Stored` = the artifact's map, `Built` =
  the per-run fallback); the `NativeCode` trait (`enter(rt, module, func, pc)` + `module_blocks` +
  `labels`) and its `BlockEntries` impl; `enter_compiled`/`block_entries`.
- `runtime.rs` — the block ABI (`HALT`=`-1`/`ERROR`=`-2`/`ENTER`=`-3`/`LOCAL_DECLINE`=`-4`/`WAIT_BIT`,
  `encode_target`/`decode_target`/`encode_wait`/`decode_wait`, `JitRuntime` — including its
  `entry_module` field, the run's context-`0` module handed to the BIF-dispatch and raise funnels and
  never read by generated code, and its `literals_base`/`literals_end` publication —
  `RUNTIME_{MACHINE,XREGS,YBASE,YEND,LITERALS,LITERALS_END}_DISP`) and the helpers: `jit_step_one`
  (per-instruction deopt → `Machine::step_instruction_with_entry`; a `Step::Enter` returns the `ENTER`
  word with the target in `JitRuntime::enter`), `jit_ext_call` (the native
  `call_ext`/`call_ext_only`/`call_ext_last` arm: import resolution through the host seam plus the
  variant's frame discipline — the `Continuation::Address{func, pc+1, context}` store, nothing, or
  `pop_frame_checked` — handed to the driver as `ENTER`), `jit_step_ext` (the `call_ext*` deopt
  carrying the host's **already-computed** seam answer into `Machine::call_ext_dispatch`, so a declined
  import is resolved **once**, not twice), `fold_step` (the single `Step`→block-ABI encoder both
  `jit_step_one` and `jit_step_ext` end in), `resolve_ext_entry`, `jit_local_call`/`probe_local_call`
  (the local-call *as-BIF* seam probe, answering `LOCAL_DECLINE`), `decorate`, `jit_tail_call`
  (`Machine::pop_frame_checked` for a linked local `call_last`),
  `jit_deallocate`/`jit_return`/`jit_deallocate_return` (the frame-discipline helpers: the checked pop
  and/or the continuation read, encoding the instruction's own outcome as the block-ABI word — sharing
  `return_word` for the continuation→word mapping and `report_frame_error` for the typed diagnostic, with
  the `FrameExitHelper`/`ReturnHelper` fn-pointer types), `jit_allocate`/`jit_allocate_heap`/
  `jit_test_heap`/`jit_trim` (the **heap / frame-admission** family: the interpreter's own
  `Machine::push_frame` / `ensure_heap` **then** `push_frame` / `ensure_heap` / `trim` calls — in the
  interpreter's own order — each answering `encode_target(func, pc + 1)` or `report_frame_error`'s
  `ERROR`, with the `FrameSizeHelper`/`HeapNeedHelper`/`AllocateHeapHelper` fn-pointer types),
  `jit_set_position` (`Machine::set_current_code`, the block prologue's position store),
  `jit_publish_y` (`jit_set_position` + `JitRuntime::refresh_y_base` in one call — a Y-accessing
  block's prologue publish) and `jit_refresh_y` (the gated post-helper window refresh); plus
  `JitRuntime::refresh_literals`/`literal_bounds` (the literal-area publication `emit/move.rs`'s
  literal guard reads) and the `NativeBlock`/`StepHelper`/`TailHelper`/`PositionHelper`/`RefreshHelper`
  type aliases.
- `call.rs` — `jit_call_store` + `CALL_STORED`/`CallOutcome`: the frame-preserving local `call`'s
  continuation store *and* the local-call seam probe in one helper call, in the interpreter's own
  probe-then-store order. `CALL_STORED` (`-5`) is consumed inside the emitted block and never reaches
  the driver's decoder.
- `family.rs` — `jit_family`: the shared helper for every family-owned instruction. It records the
  position (`machine.set_current_code`), resolves the instruction, and runs the identical
  `crate::families::dispatch(machine, entry_module, module, labels, instr, func, pc, ctx)` call the
  interpreter's per-instruction step runs, then `fold_step`.
- `test_helper.rs` — the comparison-`test` helper (`jit_test`, `TEST_TRUE`/`TEST_FALSE`): it resolves the instruction at `(func, pc)` in the active module and evaluates a two-operand comparison `test` through `beam_term`'s `cmp`/`cmp_arith`, reporting the outcome as a boolean (the block-ABI `ERROR` word on a propagating read error) and handing anything it does not own to `jit_step_one`.
- `receive.rs` — `jit_receive`: the `send`/receive-control family's own body plus the interpreter step's position store and raise funnel.
- `emit/` — `compile.rs` (the compilation entry points `compile_module`/`compile_function`, `emit_function`,
  the continuation sub-block emitters `stores_continuation`/`sub_blocks_capacity_estimate`/`emit_sub_blocks`/
  `emit_shared_sub_blocks`, and the code-size estimates), `block.rs` (the per-instruction lowering
  dispatch `emit_block` + the compiled-block shape) with `prologue.rs` (the compiled-block prologue
  `emit_block_prologue` + its position-store helpers) and `mod.rs` (the shared wiring/constants and the
  helper-sequencing layer), + live-interval regalloc, and the per-group emitters
  (`r#move.rs`, `test.rs` (+ `test/header.rs`/`test/tests.rs`/`test/refusal_census.rs`), `select.rs`,
  `arith.rs`, `compare.rs`, `aggregate.rs` (+ `aggregate/list.rs`), `frame.rs`, `heap.rs`, `family.rs`,
  `link.rs`, `operands.rs`, `liveness.rs`, `regalloc.rs`, `regs.rs`). `Allocation::y_last`/
  `refresh_y_after` and the Y-window publish/refresh emission live in `prologue.rs`/`regalloc.rs`.
  See `emit/CONTEXT.md`.

## Key facts
- **Inline dispatch**: the per-instruction `match instr` in `emit_block`. Inlined groups: metadata no-ops,
  `Move` (in-bound x-registers, an addressable **`Y` slot** on either side behind the frame-window guard, a
  constant word, or a **literal-area term** behind the literal guard) / `Swap` (two `Loc` operands, a `Y`
  side via `emit_swap_y`), `InitYregs` (X/Y destinations), `Test` (the word-level predicates **and** the
  boxed-term header kinds, with a `Y`-slot source accepted), `SelectVal` (all-immediate alternatives, with a
  `Y`-slot scrutinee accepted), `SelectTupleArity` (a `Loc` tuple), cons readers (`GetList`/`GetHd`/`GetTl`,
  all destinations a `Loc`), `GetTupleElement` (a `Loc` source/destination), the destructive
  `SetTupleElement` (a `Loc` tuple), the small-int `gc_bif` arithmetic (`'+'/'-'/'*'` and the bitwise
  `band`/`bor`/`bsr` — an x-register **or Y-slot** operand/destination, behind the run-time frame-window
  guard), the 8 comparison BIFs, the named-BIF accessors (`erlang:hd/1`, `element/2` with a compile-time
  small-int index `>= 1`, `tuple_size/1` lowered in block through the aggregate read guards, and
  `erlang:byte_size/1`/`length/1` evaluated by the `bif_helper.rs::jit_bif_value` helper), `Jump`, the local
  tail calls (`CallLast` via `emit_tail_exit` for a compiled successor and `emit_tail_exit_to_driver` for a
  resolved-but-uncompiled one; `CallOnly` via `emit_exit`), the frame-preserving local `Call` (via
  `jit_call_store` + the `CALL_STORED` compare + `emit_exit`), the cross-module calls
  (`CallExt`/`CallExtOnly`/`CallExtLast` via `emit_ext_call`), the receive-control family
  (`Send`/`LoopRec`/`LoopRecEnd`/`Wait`/`WaitTimeout`/`Timeout`/`RemoveMessage` via `emit::emit_receive` →
  `receive.rs::jit_receive`), the whole **family-owned surface** (`put_list`/`put_tuple2`, the native-record
  set, `make_fun3`/`call_fun`/`call_fun2`/`apply`/`apply_last`, the try/catch/raise set, the float set, the
  map set, the bitstring set, `recv_marker_*` via `emit/family.rs::emit_family` → `family.rs::jit_family`),
  the frame-discipline trio (`Deallocate` via `frame.rs::emit_deallocate`, `Return` via `::emit_return`,
  `DeallocateReturn` via `::emit_deallocate_return`), and the **heap / frame-admission** family
  (`allocate` via `heap.rs::emit_allocate`, `allocate_heap` via `::emit_allocate_heap`, `test_heap` via
  `::emit_test_heap`, `trim` via `::emit_trim`) — an inlined direct helper call that runs no interpreter
  step and hands the driver the helper's block-ABI word. The heap / frame-admission four are
  **non-terminating** (the interpreter's own arm returns `Step::Next`), so each continues in-block on the
  helper's `encode_target(func, pc + 1)` word and leaves the block with that word otherwise (always the
  `ERROR` of a failed admission or a failed trim); `allocate`/`trim` touch no x-register and emit **no**
  register refill (they do re-publish the Y window on the continue arm, being frame changes), while
  `allocate_heap`/`test_heap` reach the host's heap-admission door — where a **collecting** host rewrites
  `machine.x` in place — and therefore refill through `emit_continue_check`, each reaching that door
  **exactly once** per execution with the instruction's own `heap_need`/`live`. Everything else is the `_`
  arm; `FuncInfo` is deliberately there (it must deopt so a failed head clause raises `function_clause`)
  and `CallExt*` crosses modules via a driver round-trip by design.
- **The whole family-owned surface is one arm.** `emit_block` names every variant `crate::families::dispatch`
  claims and the core arms above do not, and routes each to `emit/family.rs::emit_family` →
  `family.rs::jit_family`, which runs the identical `crate::families::dispatch` call the interpreter's
  per-instruction step runs — so the lowering is interpreter-identical by construction and can never produce
  a different result. The emitted sequence is the shared helper contract (spill, call, continuation check),
  so the family arm is exactly as cheap as the receive arm, and every un-lowered instruction keeps its
  per-instruction deopt.
- **Comparison-`test` lowering (in-block, no interpreter step).** The `Test` arm's `emit/test.rs::try_inline_test` falls through to `emit_test_helper` for a comparison the inline word/header subset declines, emitting a call to the `test_helper.rs::jit_test(rt, func, pc)` helper instead of a `jit_step_one` round trip: the six comparison kinds (`is_eq_exact`/`is_ne_exact`/`is_eq`/`is_ne`/`is_lt`/`is_ge`) at exactly two operands with a resolvable non-`NO_FAIL` fail label, whatever the operand kinds (a Y slot, a float register, a literal-area pointer, an immediate, an in-bound x-register, boxed-vs-boxed) — because a comparison must route through `beam_term`'s own `cmp`/`cmp_arith` (a boxed pointer is never word-compared; `is_eq_exact` is exact equality while `is_eq`/`is_ne`/`is_lt`/`is_ge` are the arithmetic comparison). The helper answers `TEST_TRUE`/`TEST_FALSE`; the block refills its cached x-registers and continues in-block on `TEST_TRUE`, branches to the instruction's resolved fail label on `TEST_FALSE`, and leaves any other word (the `ERROR` of a propagating read error) for the driver. A `NO_FAIL` `test` (which raises `function_clause`), an unresolved fail label, a non-comparison kind and an operand count other than two are deliberately not this lowering's and keep `jit_step_one` (the interpreter's own step).
- **Named-BIF lowering (`emit/bif.rs`, in-block, no interpreter step).** The `Instr::Bif` and `Instr::GcBif`
  arms try `emit/bif.rs::try_inline_bif` after the arithmetic and comparison shapes. Its predicates
  (`bif_shape`/`bif_uses_y`) are **purely syntactic** — the import's resolved names plus the operands'
  kinds, never the register allocation — so `emit/regalloc.rs::cacheable_touches`/`cacheable_writes` and
  `emit_block` always agree. Two routes: (a) the three pure accessors, each **reusing the aggregate
  read's own bytes** — `erlang:hd/1` is `aggregate::try_inline_get_hd`; `element/2` with a compile-time
  small-int index `>= 1` is `aggregate::try_inline_get_tuple_element` at `Index - 1` (whose
  `is_tuple` + `element < arity` guard *is* `1 =< Index =< tuple_size(T)`); `tuple_size/1` reuses
  `aggregate::emit_tuple_header_guard` and derives `make_small(arity)` from the header word it leaves
  behind — claimed from `Instr::Bif` only, which is how the corpus emits them; (b) `erlang:byte_size/1`
  and `length/1` (`Target::Value`), claimed from **either** variant (the corpus emits both as a collecting
  `{gc_bif,…}`), emitted as an `emit_helper_call` to `bif_helper.rs::jit_bif_value`, whose Rust body is the
  BIF's own semantics computed through the engine's own `read_operand`/`write_operand`:
  `bitstring_size(t).div_ceil(8)` — the byte size rounds **UP** (`byte_size(<<1:1>>)` is `1`) — and a
  proper-list count, each declining when the value would not fit a small integer. Small-int
  `band/2`/`bor/2`/`bsr/2` are lowered by `emit/arith.rs::fast_arith` instead (`band`/`bor` as a raw
  **tagged-word** `and_rr`/`or_rr`, exact for negatives with no untag/retag; `bsr` untag → `sar_ri` →
  range-check → retag, claiming only an **immediate** small-int count in `0..64`). No lowering here
  modifies the driver, the block ABI or a helper; `erlang:self/0` and `erlang:node/0` (the `bif0` pool)
  are host-owned (`self_pid`/`local_node` behind `ExecutionContext`) and still deopt — the `Bif0`
  variant has no arm at all.
- **Deopt sequence**: `emit_step_and_continue` = `emit_helper_call` (**spills first**, then `rdi` = the `rt` slot, the argument words in `rsi`/`rdx`/`rcx`/`r8`/`r9`, `call r11`) + the shared `emit_continue_check` (`cmp rax, encode_target(func,pc+1)`; equal ⇒ `fill` and fall through in-block, else `jcc Ne, done`). `emit_ext_call` (the `CallExt*` arm) reuses both: import resolution + frame discipline happen in `jit_ext_call` natively, and only the helper's `ENTER`/error/`Wait` words leave the block. Every compile-time refusal in `emit_block` routes through `emit_deopt`, which records the static-census deopt (test builds) and then runs `emit_step_and_continue`. The `bif_helper.rs::jit_bif_value` route reuses the same pair, so a value-BIF fast path, a declined value shape and a deopt are all the identical helper contract — the only difference is the word the helper returns.
- **Driver entry** (`run_native_outcome`, `run_native_label_outcome`): `enter_machine` (reset, seed `x[0..arity)`, `push_frame(0)`, `set_literals`, then the module-wide label map — **reused** from the artifact when it holds one (`RunLabels::Stored`, the map built with the artifact), built only otherwise (`RunLabels::Built`)) then `machine.enter_run(module, func)` then `drive`. The driver builds the `JitRuntime` (null literal base/end) and calls `rt.refresh_literals()` before the loop and again after every `Step::Enter`/`ENTER` arm, so the emitted literal guard always sees the active module's decoded literal area. `drive` latches `machine.trace_on`, `machine.bif_in_machine_on` (the cached `ExecutionContext::wants_bif_in_machine` answer, the same gate the interpreter's `Machine::drive` latches, so the in-machine BIF-dispatch door is taken identically on both tiers) **and** the reduction gate (`machine.reduce_on` = `wants_reduce()`, with `machine.reductions` zeroed at the entry — one `drive` call is one scheduling slice, the rule the interpreter's `Machine::drive` applies too), loops on `code.enter(rt, active_module, func, pc)`, and hands any address the tier has no compiled block for to one `Machine::step_instruction_with_entry(entry_module, …)` — `JitRuntime::entry_module` is the run's context-`0` module (never read by generated code), which the deopt helper threads to the BIF-dispatch funnel and the raise funnel.

  **Every driver-loop iteration charges one reduction unit at the block entry**, before `code.enter(...)`: the loop publishes the position with `Machine::set_current_code(func, pc)`, bumps `Machine::reductions` (saturating, in the same step as the hook, so the two always agree) and, when the latched gate is on, calls `ExecutionContext::reduce(1)?`. That is the native tiers' first and only pre-emption point; a host whose `reduce` is the default no-op declares `wants_reduce() == false` and pays one `bool` load per iteration and nothing else (byte-identical behaviour), while the engine-side counter is bumped either way, exactly as in `Machine::drive`. The position is load-bearing: the runner parks an exhausted machine at `Machine::current_code` and re-enters there, and at the block entry the published position **is** the address about to run with the block not yet executed, so a refusal parks exactly there and the resume replays no prefix (the address's instruction runs exactly once). A refusing charge from the call-resolution/BIF seams would instead refuse with the block's *entry* position current only **after** a prefix of the block had run, and the resume would replay that prefix (measured: `beam_rt -eval 'halt(0).'` then exits 3 with `unsupported: opcode: run: process blocked`), which is why those seams stay spend-only. One iteration is one **driver round-trip** — a natively executed block, or the per-instruction interpreter step the driver takes for an address no block covers — so the unit is a block entry, not an instruction: a synthetic loop that is one block per instruction is priced exactly like the interpreter (3 charges per iteration for `label`, `bif0`, `jump`), while a **natively linked local tail-call chain** (`call_last`/`call_only` linked between compiled blocks) never returns to the driver and is therefore priced **once** — `slice.beam`'s tail-recursive `fib/1` at n=18 costs **2** charges against the interpreter's **152**. That is the honest resolution limit of the block-entry charge and the tier's remaining pre-emption residual: such a chain is priced once and, having no refusing seam inside it, cannot end its slice. Pinned by `tests/jit_reductions.rs`.
- **Compile-on-entry + layout-dependent register guard** (`enter_compiled`): `CompiledModule::native_entry(module, func, pc)` returns the block's entry together with the `highest_native_register` bound the artifact's layout requires (compiling the function first for a lazy artifact). The block is entered only when the running machine's `x` file covers that bound (`x.len() > bound`); otherwise, or when the address has no compiled block, `enter_compiled` returns `None` and the driver takes exactly one interpreter step. The bound is layout-dependent: an **eager** artifact reports the **whole-module** max (its seal-time links tail-call directly into other functions' blocks inside the one shared mapping, so the entered function's own bound would not cover the linked code), while a **lazy** artifact reports the entered **function's** own bound (each function owns its mapping and a cross-function transfer round-trips through the driver, which re-checks the target function's bound on its own entry).
- **Resume driver** (`run_compiled_resume_outcome`): the **no-reset** counterpart of `run_compiled_outcome` (it does NOT call `enter_machine`). It mirrors `Machine::resume_outcome` — `machine.enter_run(module, func)`, the artifact's own label map (`compiled.labels()`, built once at compilation), then the shared `drive` entering the compiled block at the reported `(func, pc)` — so a slice suspended on the baseline tier resumes on it with interpreter-identical semantics. The reduction counter restarts per slice: the shared `drive` zeroes `machine.reductions` at its entry (one `drive` call is one scheduling slice, the interpreter loop's own rule) and `run_compiled_resume_outcome` additionally calls `machine.reset_reductions()` before entering, so a resumed slice's `Machine::reductions()` counts only that slice's own block entries, exactly as the interpreter's resume counts only its own instructions. A non-entry module context (a suspension inside a cross-module callee) is served the same way the interpreter's resume serves it: `drive` reads the context from `Machine::current_context`, lands in it (`switch_into` re-binds the callee's module, label map and literal area and resolves its blocks through `NativeCode::module_blocks`), and re-enters the callee's own `(func, pc)`.
- **Cross-module driver** (`drive`): a `Step::Enter` (from its own interpreter step, or the `ENTER` word a block hands back) switches context through `switch_into` — `Machine::switch_context`, re-bind module/labels/literals (`rt.refresh_literals()`), and the callee's blocks via `NativeCode::module_blocks` (the `BlockEntries` impl consults the per-thread `CodeCache`, staging the callee as a **lazy** artifact on first sight) — then continues in the callee's compiled code (its functions compiling on first entry), or on the per-instruction step when the tier has no blocks for it. The context switch is a driver round-trip by construction (each `CompiledModule`/`CompiledFunction` owns a separate executable mapping and `JitRuntime::module`/`labels` are baked for one module), but the `call_ext*` instruction itself is executed natively by `jit_ext_call`, so only the callee's entry goes back through the driver. A **traced** run refuses native entry and hands the slice to `Machine::drive`.
- **Variable-length native frame** (`emit/mod.rs::native_frame_regs` + `emit/mod.rs::frame_slot_len`, torn down by `emit/link.rs::emit_frame_teardown`). A block saves in its prologue, and restores in its teardown, **only** `rbx` (always: the `machine.x` base) plus the machine registers it actually holds as cache slots — not the canonical six-register callee-saved set (`emit/mod.rs::SAVED_REGISTERS`, still the canonical set the block ABI may touch, pinned against every per-block subset by a `debug_assert!` in `native_frame_regs`). The `rt` slot is sized `8` bytes when `1 + slots.len()` is even and `16` when odd, so `rsp` is 16-byte aligned at every helper call site for the pushes the block actually makes. A block with no cache slots (the majority) therefore performs **no** callee-saved push/pop at all. Deterministic corpus counters (`counters.rs`, grand total over the generically-discovered installed-OTP corpus, 1353 modules / 103 008 functions): `blocks = 597 333`, `cache_slots = 756 632`, push/pop ops **7 167 996 → 2 707 930** (−62.2 %: `blocks + cache_slots` per direction instead of `6 · blocks`), `emitted_bytes` **320 065 790 → 308 853 108** (−3.50 %), with `ir_instrs = 3 585 490`, `written_slots = 621 058`, the per-block cache-size histogram and every fill/spill **count** unchanged (the lever changes no register allocation). Throughput effect: **none measurable** — see `## Known Issues`.
- **The prologue loads only the block's live-in cache slots** (`Allocation::entry_fill`, produced by `emit/regalloc.rs` from the entry set of `emit/liveness.rs::fill_sets`, consumed by `emit/block.rs`'s prologue through `Allocation::entry_loads`). A cached register the block writes before it reads is never loaded at entry (that writing instruction sets it), and the per-block cache is self-contained — every block and continuation sub-block loads its own cache, so nothing is inherited from a predecessor's physical register. The read set is per-shape (`cacheable_reads` + each lowering's `reads()`), and a read-modify-write shape (`Swap`, an in-place destination that is also an operand) reports both sides as read, so the set is deliberately conservative; an absent table (a synthetic `Allocation` a unit test built directly) falls back to the whole cache. Deterministic corpus counters: prologue loads **756 632 → 352 537** (−53.4 %) and `emitted_bytes` **308 853 108 → 306 024 443** (−0.92 %), with `blocks`/`cache_slots`/`written_slots`/`ir_instrs` and both per-block histograms unchanged. Cumulative with the frame lever: push/pop ops 7 167 996 → 2 707 930, emitted bytes **320 065 790 → 306 024 443** (−4.39 %).
- **Block prologue position store / Y-window publish**: a **Y-free** block calls `jit_set_position(rt, func, block_start_pc)` after its own frame-register pushes (`native_frame_regs`) and the `rt`-slot setup and before loading the cached x-registers, so a linked chain (which never returns to the driver) still advances `Machine::current_code` at block granularity. A block whose code **accesses a Y slot** (`Allocation::y_last.is_some()`) calls `jit_publish_y(rt, func, pc)` **instead** — the same position store *plus* the current frame's Y-window refresh in one call, so publishing costs such a block nothing extra.
- **Per-block Y-window publication** (the Y operand/destination axis every widened lowering shares). The window is the current top frame's `[y_base, y_end)` in `machine.stack`, held in `JitRuntime::{y_base,y_end}` at `RUNTIME_YBASE_DISP`/`RUNTIME_YEND_DISP`; `jit_publish_y`/`jit_refresh_y` refresh it via `JitRuntime::refresh_y_base`. Publication is **per block and compile-time gated** on `Allocation::y_last` (the highest pc of a block whose instruction accesses Y, derived from `arith::uses_y` — the union of every lowering's per-shape Y test): a Y-accessing block publishes in its prologue (above) and, after an **in-block** helper call, emits `jit_refresh_y` **only when a later instruction of the block still accesses Y** (`Allocation::refresh_y_after(pc)` = `matches!(y_last, Some(last) if last > pc)`); a Y-free block keeps the plain position store and pays literally nothing. The emitted guard is a pure rejection (`jae` to `jit_step_one`), and `y_base == y_end == null` (no live frame) rejects every slot. An index the emitted `lea ... disp32` cannot address (`emit/operands.rs::y_addressable`) is refused at **compile** time. The lowerings that read or write the window are the small-int arith fast path, an inlined `Move`/`Swap` with a `Y` side, `InitYregs` with a `Y` destination, the cons/tuple readers and the `SetTupleElement` writer with a `Y` source/destination/tuple, `SelectTupleArity` with a `Y` tuple, and a `Y`-source `test`/`select_val`; each refuses the operand at compile time when the `Y` index is un-addressable, and each shares the guard emission in `emit/operands.rs` (`emit_guarded_y_address`/`_load`/`_store`, `Loc::load`/`guard_store`). Non-`Y` windows are never the access path: an instruction with a `Y` operand a given lowering does not claim deopts to the interpreter, which reads `machine.stack` itself. A Y slot is never charged to the x-register usage counts / `highest_native_register`.
- **Literal-source `Move` guard**: a `Move` whose source is a literal-area term is lowered behind a **run-time** check against `JitRuntime::literals_end` (never a baked pointer), because a cached block can outlive the `Arc<Module>` it was compiled for; a rejection deopts to the interpreter step, which reads the literal area itself.
- **Register-cache contract** (the `Allocation` and its spill/fill traffic live in `emit/regs.rs`).
  A block caches a **fixed** set of x-registers for its whole duration (`Allocation::slots`, the
  `emit/regalloc.rs` live-interval-ranked choice — an x-register is either *always* held in its
  machine register or *never* cached, so the two representations can never disagree).
  Both directions of the traffic are **partial**: `spill(buffer, allocation, site, pc)` stores back
  only `Allocation::spill_slots_at(pc)` — the cached registers **dirty at that pc**
  (`Allocation::spill_sets`, the `emit/regalloc.rs` forward dirty walk; the empty-table fallback is
  the whole `Allocation::written` set) — and `fill` reloads only `Allocation::fill_slots(pc)` — the
  registers the continuation can still observe, the block-local liveness of
  `emit/liveness.rs::fill_sets` unioned with the write set, `(live ∪ written) ∩ slots` — while the
  prologue load is the block's **live-in** slot set (`Allocation::entry_fill`, `FillSite::Prologue`), so a
  cached register the block writes before it reads is never loaded at entry.
  A cached register outside the spill set still holds exactly its `machine.x` value at every program
  point (so storing it would write back an unchanged word), and a cached register outside the fill
  set keeps a stale word that is only ever overwritten, never read (the write set is always in the
  fill set).
  Every helper-call `call_r` site (`emit/mod.rs::emit_helper_call`, which passes the instruction's
  own `pc` as the store pc, and `emit/link.rs`'s three exits `emit_exit`/`emit_tail_exit`/
  `emit_tail_exit_to_driver`, which carry a trailing `store_pc` separate from their branch target)
  spills first.
  Non-cached x-registers are addressed directly in `machine.x`.
- **Continuation sub-blocks — BOTH layouts.** A **continuation address** is the `pc + 1` a frame-preserving call stores in the live frame — one of `Instr::Call`, a non-tail `Instr::CallExt`, `Instr::CallFun`, `Instr::CallFun2` or `Instr::Apply` (`emit::stores_continuation`; the tail forms store no new continuation). The loader cuts a function into blocks only at `Instr::Label`s, so such an address generally lies **inside** a block. Both layouts lower one sub-block covering `[p, sub_block_end(...))` — the containing IR block's `end` capped at the next continuation address, so the ranges are disjoint and the total is linear — through the very same `emit_block` an IR block uses, so a sub-block is interpreter-identical by construction:
  - the **lazy** layout: `emit::compile_function` calls `emit_sub_blocks` after `emit_function`, storing the entry in `CompiledFunction::sub_entries`;
  - the **eager** layout: `emit::compile_module` calls `emit_shared_sub_blocks` **after every** IR block (so the block regions stay contiguous at the front of the shared mapping), storing the entry in `EagerLayout::sub_entries`.
  Both passes consult their `entries` table **first** and `sub_entries` second (a real block start always wins), so `Summary::block_count`/`code_map`/`code_address_range` keep describing IR blocks only; a sub-block reserves **no** entry label (nothing branches to a continuation address — the driver re-enters there after a deopt or a `Wait`) and it starts with a full block prologue. An address that is already a block start, or at/past the end of the function, needs no sub-block. **Both** passes derive the allocation for the range they emit — `allocate_registers(module, code, start, end)` — because an `Allocation` is entry-point-relative (`block_start`, the per-`pc` fill/spill tables, the live-in set): a sub-block is a fresh native function whose prologue loads only its own live-in cache, so the containing IR block's allocation would leave a register the block writes before `start` unloaded and would carry its dirty bits. The lazy pass folds the sub-block touches into the function `Summary` as a **superset**; the eager pass discards them, so the whole-module `Summary` stays exactly the IR blocks'. Both add `emit::sub_blocks_capacity_estimate` to the buffer estimate, so the extra code can never under-allocate the `CodeBuffer` (an under-estimate would be a `SystemLimit` compile error).

## Known Issues
- **The tier's per-slice cost on a message-passing workload is the entry-module resolution, NOT the deopts — and for a real `.beam` that resolution is O(1).** Every `run_baseline_cached*` slice entry/resume resolves the entry module through `CodeCache::compiled_handle(&Module)` (`../cache/mod.rs`), whose `ModuleKey::of` is a **two-variant** identity: `Digest([u8; 16])` = the module's **load-time MD5** (`Module::checksum`, read as a plain `Copy` — no hash, no allocation) for a real `.beam`, else `Content { name, fingerprint }` over the whole-IR hash (`module_fingerprint`, a full `Debug`-render hash of the decoded IR) for a synthetic all-zero-checksum `Module::default()`. A real `.beam` therefore resolves with **zero** IR hashes on every slice, primed or not, and only a synthetic module can fall to `module_fingerprint` per slice; the address memo (filled by `cached_module_blocks(&Arc<Module>)` on the cross-module *callee* path and by the host-facing `cache::remember_module(&Arc<Module>)` on the *entry* path) is a redundant micro-optimisation on that digest path. The interpreter tier never touches `CodeCache`. Measured by the two-process copy-bound probe (`tests.rs::the_entry_module_priming_is_measured_on_both_tiers`, both tiers named, with `/proc/loadavg` and the per-pass `module_fingerprint` count): the probe's **synthetic** modules take the `Content` fallback, so unprimed they re-hash per slice (the probe's printed ≈6.8×-of-interpreter reading) and primed they read ≈0.8× with **zero** hashes — both are **single-run estimates from that probe, not pinned results** (`tests.rs::the_entry_module_priming_is_measured_on_both_tiers` asserts the `module_fingerprint` counts only; the authoritative tier ratios live in `../../../MEASURED.md`) — the tier's own work is parity-or-better and the whole gap is that hash; a real `.beam` needs no priming to reach the same zero-hash result (`cache/tests.rs::a_real_loaded_beam_never_rehashes_its_ir_on_any_entry_path`). `remember_module` is the entry-path priming API and has **no production caller** — its only callers are tests (`jit/baseline/tests.rs`, `jit/cache/tests.rs`); a host would call it once per module it enters, and the tier cannot pre-empt it (`compiled_handle` is called before any `baseline::run_compiled*` body). Deopt inlining — including the natively-lowered receive family and the family arm — does **not** move the wall-clock on this shape: the deopt chain is cheap relative to that hash. (The lazy `native_entry` per-block lookup is an O(1) `RefCell` probe + `Rc::clone`, no hash/scan; the driver's one charge per **block entry** against the interpreter's one per **instruction** is a real unit divergence, but ≈1 ns per iteration.)
- **Both levers reduce counts but have no measurable throughput effect on this host (honest negative).** Interleaved A/B (before = the pre-lever tree, after = the variable-length frame + the live-in prologue fill; 9 paired rounds with the order reversed between rounds, `bench run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound`, one benchmark process at a time, `/proc/loadavg` 8–123 recorded at every run): every case's paired after/before ratio flips sign across rounds and each side's own run-to-run spread is 1.5–5.2×, so no case clears the spread (the min-over-N ratios 0.86–1.98 are confounded, because the two minima come from different rounds). The same harness *can* resolve a per-instruction difference: the tier ratio (default JIT ÷ `BEAM_RT_TIER=interpreter`) is stable and reproduces `tools/bench/RESULTS.md` §5 (`list_walk` **6.87×** here vs 6.86× there; the default tier is cheaper on 9 of 10 cases), so a ≲2 % wall-clock effect — the counters are −62.2 % push/pop ops, −53.4 % prologue loads, −4.39 % emitted bytes — is below this host's resolution. Do not re-run this A/B expecting signal on a loaded host; the levers' evidence is the deterministic emitted-instruction/byte reduction.
- **`emit/block.rs` (993 lines) holds the single per-instruction lowering dispatch** (`emit_block`, one `match` arm per instruction group plus an `emit_deopt` fallback), which is what makes it cohesive — splitting it further would scatter one decision across files or need an abstraction the emitter does not want; the compiled-block prologue it emits first lives in `emit/prologue.rs`. `emit/mod.rs` (401) is the shared wiring/constants and helper-sequencing layer.
- **Pre-existing files above the ~1000-line guidance, none of them a single decision the way `emit/block.rs` is**: `runtime.rs` (1483) is the block ABI plus every per-instruction helper, which all read the one `JitRuntime` layout; `mod.rs` (1116) is `BaselineJit` + the driver + `NativeCode`; `census.rs` (1583), `bif_tests.rs` (1744) and `tests.rs` (6245) are `#[cfg(test)]`-only. Splitting any of them is a separate, parity-neutral refactor.
- **`counters.rs` is `#[cfg(test)]`-only**, so only a unit test under `src/jit/baseline/` can reach it; `census.rs` and `emit/test/refusal_census.rs` are the only non-`tests.rs` callers. A warmed `CodeCache` re-emits nothing, so a counter pin must compile cache-free through `emit::compile_function` (as `call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt` does).

## Constraints
- `#![allow(unsafe_code)]` on `mod.rs`/`runtime.rs`/`call.rs`/`family.rs` and `emit/*` (JIT zone); every `unsafe` block carries a `// SAFETY:` rationale, and the safe-only modules (`receive.rs`, `test_helper.rs`, the test submodules) stay under the crate-level deny.
- Never panic / never a wrong result: an unsupported shape is the typed `error::unsupported`.

## Test Strategy
`tests.rs` unit-tests the ABI encoding, the `#[repr(C)]` runtime layout, regalloc, the fast-path constants/arithmetic, the deopt/yield folding (a deopted `wait` folds to `RunOutcome::Wait`), and that a collecting `test_heap` reaches the host's admission door exactly once and reloads the cached registers afterwards. The **heap / frame-admission** family has its own pins: tier agreement for `allocate` (both `HeapNeed` encodings)/`allocate_heap`/`test_heap`/`trim`, an over-trim raising the interpreter's own typed diagnostic on both tiers, the exactly-once admission (with the instruction's own `heap_need`/`live`) for the two heap-admitting members on each tier, and the structural claim that none of the four records a static or a run-time deopt. It also pins that the comparison-`test` lowering leaves no `Test` entry in the two-process copy-bound probe's per-opcode `jit_step_one` table. `the_y_window_traffic_of_the_committed_fixtures_is_measured` first pins the Y-traffic instrument on a synthetic shape with known traffic (2 loads / 2 stores / 2 same-block slot reuses), then counts the committed `erlc` fixtures: **12 guarded Y accesses over 235,977 emitted bytes**, 9 `Y`-touching blocks and **1** same-block slot reuse — the measured bound on what a per-block single-slot Y-value cache could save. Tier equivalence on real modules is in the crate's `tests/jit*.rs` (see `../CONTEXT.md`), which drive the **lazy** path through `Machine::run_with_tier`.

The node's newer suites are `#[path]` submodules of `tests.rs` (so they keep `tests.rs`'s shared fixtures and helpers) plus one submodule of `emit/test.rs`:
- `call_tests.rs` — the frame-preserving local `Call` (`the_frame_preserving_local_call_agrees_with_the_interpreter`, a two-argument and a nested chain, `the_frame_preserving_local_call_records_no_static_deopt`, `the_lowered_call_still_gives_its_continuation_a_native_sub_block`, an undefined-label deopt, no register touch/write, and `the_call_store_sentinel_is_disjoint_from_every_other_block_word`). `the_eager_sub_block_reads_the_call_result_out_of_machine_x` is the **eager-layout** pin (helpers `call_result_passthrough_case`/`eager_sub_block_context`): on the `f(A,B) -> g(B)` shape — a mid-block frame-preserving `call` followed by two **unguarded** inlined `move`s that carry the result across the continuation — it compiles the **eager** artifact through `JitBackend::{compile, run_compiled}` on `BaselineJit` (the tier entries go through `cache::run_baseline_cached*`, i.e. the **lazy** artifact, so they cannot reach the eager sub-block layout at all) and asserts the result equals both the interpreter's `Machine::run_export` and the hand-computed value; the callee is cache-free (a non-lowered `bif`) so no callee register can mask a stale one. It first pins the premise that the containing IR block's allocation and the sub-range allocation differ — `containing.slot(0).is_some() && sub_range.slot(0).is_some()`, `!containing.entry_loads(0)`, `sub_range.entry_loads(0)` — so a re-collapse of the eager pass onto one allocation fails on the shape itself, not only on the value.
- `family_tests.rs` — the family-owned constructors (`the_family_owned_constructors_agree_across_the_tiers`, a family raise identical on both tiers, `the_family_owned_constructors_are_lowered_without_a_jit_step_one_deopt`, and a family-built term reaching a later native read through the cache refill).
- `tail_exit_tests.rs` — the unlinked tail-call path (`emit/link.rs::emit_tail_exit_to_driver`: forward/backward/self tail calls, a raise inside the tail callee, a frame-size mismatch, an undefined label keeping the interpreter's error, and `the_unlinked_tail_exit_records_no_call_last_deopt`).
- `y_operand_tests.rs` — the Y-operand `test`/`select_val`/`select_tuple_arity` lowerings, asserting tier agreement and the `Loc` gate (`the_y_gate_is_true_for_a_y_source_and_false_for_an_x_source`).
- `operand_tests.rs` — `the_move_deopt_breakdown_over_the_corpus_is_printed`, the move-shape deopt census.
- `bif_tests.rs` — the named-BIF fast paths (its own `bif_host()` supplies the interpreter tier's BIF semantics, since no BIF is engine-native). Per lowered accessor a tier-equivalence value test plus its deopt shapes: `hd_is_lowered_to_the_cons_read` / `element_is_lowered_to_the_tuple_read` / `tuple_size_is_lowered_to_the_tuple_header_read`, with `the_declined_bif_shapes_raise_the_interpreters_badarg` (non-cons, non-tuple, `element(0, T)`/`element(9, {a})`, non-immediate index); `the_bitwise_and_shift_gc_bifs_are_lowered_to_the_bifs_value`, `the_bitwise_fast_path_agrees_with_the_bif_over_a_small_grid`, `the_bsr_count_bounds_are_exact_on_both_tiers` (`0`/`64`/negative/register counts), `the_declined_bitwise_and_shift_shapes_are_the_interpreters_outcome` (bignum/float); `byte_size_is_lowered_to_the_bit_size_rounded_up` / `length_is_lowered_to_the_proper_list_walk` / `the_collecting_value_bifs_are_lowered_on_both_tiers`, with `the_declined_value_bif_shapes_raise_the_interpreters_badarg` and `the_declined_collecting_value_shapes_raise_the_interpreters_badarg` (non-bitstring, improper list, a bignum constant); the Y-operand shapes (`the_y_operand_shapes_are_lowered_and_agree_across_the_tiers`, `the_value_bif_y_operand_shapes_agree_across_the_tiers`, `the_bitwise_gc_bif_y_operand_shape_agrees_across_the_tiers`); and the three structural pins `the_lowered_bif_fast_paths_record_no_static_deopt` / `the_lowered_value_bif_fast_paths_record_no_static_deopt` / `the_lowered_bitwise_gc_bifs_record_no_static_deopt`.
- `emit/test/refusal_census.rs` (`#[cfg(test)] mod refusal_census` from `emit/test.rs`) — `the_refusal_attribution_census_is_printed`, attributing `test`/`select_val`/`select_tuple_arity` refusals to exact predicates/operand shapes and listing the `bif`/`gc_bif`/`bif0` deopt pools by BIF name. The pools are **self-maintaining**: `already_inlined(module, instr)` consults the emitters' real shape predicates (`comparison_shape`/`fast_arith`/`bif_shape`) rather than a name allowlist, so a newly lowered name leaves its pool automatically (the `bif0` pool has no arm to consult — nothing lowers `Bif0`).

### The deopt census (`census.rs` + `counters.rs`)
**A test-only static + dynamic deopt census** ranks which `beam_loader::Instr` opcodes the
tier still fails to lower — the evidence a widening of the inline set is chosen from
(no production cost: both modules are `#[cfg(test)]`, and every call site is individually
`#[cfg(test)]`-gated). Four independent views:
- **Static** (`census.rs::the_static_deopt_census_over_the_corpus_is_printed`): scores the
  **compile-time** dispatch decision deterministically. `counters.rs::record_instr_instance`
  (the emitter loop's top, `emit/block.rs`) counts every instruction the lowering dispatch
  visits per variant, and `counters.rs::record_instr_deopt` — reached through the
  `emit/block.rs::emit_deopt` wrapper that **every** `try_inline_*` refusal and the `_` arm
  call instead of `emit_step_and_continue` directly — counts the ones it did not lower.
  (The run-time guard fallbacks *inside* the `try_inline_*` functions keep calling
  `emit_step_and_continue`; they are run-time deopts, attributed by `deopts_by_variant`, not
  static ones.) The corpus is the crate's `tests/fixtures/*.beam` (discovered by directory
  scan, no hardcoded names) plus the installed OTP `.beam`s discovered generically from
  `erl`'s `code:lib_dir()` (**no hardcoded app names**; `ERTS_ORACLE` honoured; a bounded
  sample via `OTP_MODULE_CAP`, default 4096 covering the whole tree; the OTP part is skipped
  loudly without a usable `erl`). It compiles each function through `emit::compile_function`
  after `crate::jit::cache::clear_baseline_cache()`. Prints a **raw deopt-count** ranking
  (`variant | deopts | instances | deopt_ratio`) and a **weighted-score** ranking
  (`score = deopts * deopt_weight`, where the block-terminating dispatch opcodes `Jump`/
  `SelectVal`/`SelectTupleArity` weigh 4 because a deopt there also forfeits the direct
  block-to-block link — see `deopt_weight`'s rationale).
- **Runtime** (`census.rs::the_runtime_deopt_census_over_the_hot_shapes_is_printed`): runs
  `crate::jit::bench::assert_tier_agreement()` with recording on and dumps the per-variant
  `Counters::deopts_by_variant` the executed blocks produced — which un-lowered opcodes
  dominate the **hot** shapes.
- **Dynamic per-variant execution** (same test, immediately after its deopt ranking):
  `crate::jit::bench::run_workloads_on(Tier::Interpreter)` with `crate::exec_census` recording on —
  the **executed** population itself (per `Instr` variant, plus the typed errors per variant with the
  first error text) over the same in-crate workload set. It runs on the **interpreter** tier on
  purpose: a native tier emits a lowered instruction as native code, so its funnel only sees the
  steps it deopted (exactly the population the deopt ranking already covers). The caveat the section
  documents: this is the in-crate tier workload set, **not** the OTP corpus (a corpus-scale dynamic
  view is a `tests/` sweep's business). Asserts only liveness/consistency (non-empty map, both
  totals = their map's sum, a first-error text per erroring variant, non-empty variant keys, a
  ranking that faithfully re-orders the map) and writes its own `exec_census::write_dump("dynamic")`
  TSV, tolerating a write failure like the deopt dump.
- **Emitted byte size** (`census.rs::the_per_kind_emitted_code_size_table_is_printed`): for the
  in-block lowerings already present (`Move` incl. the literal-area source, the Y-operand
  `test`/`select_val`/`select_tuple_arity` shapes, `Call`, the unlinked `CallLast` tail exit, the
  family arm, the named-BIF accessors) it compiles each shape as a single-IR-block function through
  `emit::compile_function` (counters on) and prints its whole-block emitted byte length against a
  one-`func_info`-block **deopt reference** (199 B whole block; the `2N − N` marginal isolates the
  60 B per-occurrence deopt sequence). Per-kind attribution is exact because each shape compiles as
  its own single-block function; it also prints the whole-artifact `code_len` of the committed
  fixtures.
All four write a machine-readable TSV to `std::env::temp_dir()` and assert only liveness/consistency
(never a threshold), so the printed rankings are the evidence and the tests pass as the inline set
changes. Run the static census with
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_static_deopt_census_over_the_corpus_is_printed -- --nocapture`
and the emitted-byte table with the same command and `--lib the_per_kind_emitted_code_size_table_is_printed`.

## Routing Table
- `code.rs` → the artifact in its two layouts (`CompiledModule`/`CompiledFunction` [`entries` + `sub_entries`]/`EagerLayout::sub_entries`/`Summary`) + the compile-on-entry point.
- `emit/` → the compilation entry points + the per-instruction emitters (`compile.rs` `compile_module`/`compile_function`/`emit_function` + the continuation sub-blocks `stores_continuation`/`sub_blocks_capacity_estimate`/`emit_sub_blocks`/`emit_shared_sub_blocks` + the code-size estimates; `block.rs` (the per-instruction lowering dispatch `emit_block`) + `prologue.rs` (the compiled-block prologue `emit_block_prologue`) + `mod.rs` (the shared wiring/constants and helper-sequencing layer), `r#move/test/select/aggregate/arith/compare/bif/frame/heap/family/link/operands/liveness/regalloc.rs` (+ `test/header.rs`/`test/tests.rs`/`aggregate/list.rs`), `regs.rs` (the register cache: the `Allocation` + the dirty-set `spill` and partial `fill` (`spill_slots_at(pc)`/`fill_slots(pc)`) + `SpillSite`/`FillSite`), `test/refusal_census.rs` (test-only refusal attribution)) — `frame.rs` is the frame-discipline trio `Deallocate`/`Return`/`DeallocateReturn`, `heap.rs` the heap / frame-admission family `allocate`/`allocate_heap`/`test_heap`/`trim` (`emit_allocate`/`emit_allocate_heap`/`emit_test_heap`/`emit_trim` + the shared `emit_frame_change_continue`), and `family.rs` the shared arm for the family-owned surface (`emit_family`); the Y-window publish/refresh emission (`Allocation::y_last`/`refresh_y_after`), the `emit/operands.rs` readers (`x_offset`/`x_register`/`y_offset`/`y_register`/`y_addressable`/`constant_word`/`small_word`) plus the shared guarded-`Y` emission and the `Loc` abstraction (`locate_all`/`Y_ADDR_SCRATCH`/`Y_ADDR_SCRATCH_2`), the `emit/arith.rs`/`emit/r#move.rs`/`emit/aggregate.rs`/`emit/test.rs`/`emit/select.rs` Y support (`uses_y`, plus `emit/bif.rs::bif_uses_y` for the BIF accessors), and the `emit/regalloc.rs` `y_last` derivation; see its `CONTEXT.md`.
- `runtime.rs` → block ABI + `jit_step_one`/`jit_ext_call`/`jit_local_call`/`jit_tail_call`/`jit_deallocate`/`jit_return`/`jit_deallocate_return`/`jit_allocate`/`jit_allocate_heap`/`jit_test_heap`/`jit_trim`/`jit_set_position`/`jit_publish_y`/`jit_refresh_y`
  + `RUNTIME_YBASE_DISP`/`RUNTIME_YEND_DISP`/`RUNTIME_LITERALS_DISP`/`RUNTIME_LITERALS_END_DISP` + `JitRuntime::refresh_literals`/`literal_bounds`; `tests.rs` → unit tests.
- `bif_helper.rs` → `jit_bif_value` (the `StepHelper`-shaped native evaluation of `erlang:byte_size/1`/`length/1`: `byte_size_value` = `bitstring_size(t).div_ceil(8)`, `list_length` = the proper-list walk, both through the engine's own `read_operand`/`write_operand`; `read_is_allocation_free` gates the one operand kind that could allocate) + `evaluate_bif`; it reuses `emit::arith::import_names` (the one shared import→name resolver) and declines to `jit_step_one` for every shape it cannot prove. `bif_tests.rs` → the named-BIF tier-equivalence/deopt suite above.
- `call.rs` → `jit_call_store` + `CALL_STORED`/`CallOutcome`; `receive.rs` → the `jit_receive` `send`/receive-control helper; `family.rs` → `jit_family` (the shared family-owned-instruction helper); `test_helper.rs` → the comparison-`test` helper (`jit_test`, `TEST_TRUE`/`TEST_FALSE`) + its `jit_step_one` fallback for anything it does not own.
- `counters.rs` → the `#[cfg(test)]` structural counters (`Counters`, `record_*`, `snapshot`): the `jit_step_one`/`jit_step_ext` deopts (a `Test` deopt is keyed by kind — `"Test(IsEqExact)"`, `"Test(IsTuple)"`, … — in `deopts_by_variant`, so which `test` shapes still deopt is readable per predicate), the driver round-trips, the prologue position/publish calls, the **register-cache traffic** (`record_spill`/`record_fill`/`record_block`: per-block IR/byte/cache/write-set sizes + spill/fill stores by site), the **Y-window traffic** (`record_y_access`/`record_y_invalidation`/`record_block`: emitted guarded `Y` loads/stores + their emitted bytes, `Y`-touching blocks, the in-block refresh sites, and the same-block `Y`-slot reuse count) — one recorded run yields all **three** spill **store** models
  (the **dirty** set `spill` emits via `spill_slots_at(pc)`, the whole-block **written** set and the
  **full cache**) and both fill models, whose corpus-wide table is the test
  `the_register_cache_traffic_census_over_the_corpus_is_printed` — and the **static deopt census** (`record_instr_instance`/`record_instr_deopt` + the `instr_instances`/`instr_deopts` maps driving `census.rs`).
- `census.rs` → the `#[cfg(test)]` deopt census: the static corpus census (fixtures + generically-discovered OTP modules), the runtime hot-shape census (deopts **and**, from the same workloads on the interpreter tier, the `exec_census` per-variant execution ranking + typed-error table via its `dynamic_execution_census` helper), and the per-kind emitted-byte-size table, each printing a table; see `## Test Strategy` above and the file's own module docs.
