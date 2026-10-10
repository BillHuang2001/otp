# `beam_engine` native baseline JIT tier (`src/jit/baseline/`)

## Intent
The first `JitBackend` the engine builds: it lowers every basic block of a `beam_loader::Module`
into a real `extern "C"` x86-64 function and runs them through a driver loop, deopting any
instruction it does not inline to exactly one interpreter step. Correctness-first mechanism, not an
optimiser (DeepDive §F): every natively-executed result is interpreter-identical by construction.
Backend behind `Tier::BaselineJit`; `jit-beamasm` aliases this tier.

## API Surface
- `mod.rs` — `BaselineJit` (zero-sized `JitBackend`); `CompiledModule` (caller-owned artifact: `entry`, `block_count`, `cached_registers`, `highest_cached_register`, `highest_native_register`, `code_len`, `code_address_range`, `code_map`); the driver `run_compiled`/`run_compiled_outcome`/`run_compiled_label_outcome`, the no-reset `run_compiled_resume_outcome`, and the shared `enter_machine`/`run_native`/`run_native_outcome`/`run_native_label_outcome`/`drive`; the `NativeCode` trait (`BlockEntries`).
- `runtime.rs` — the block ABI (`HALT`/`ERROR`/`WAIT_BIT`/`ENTER`, `encode_target`/`decode_target`/`encode_wait`/`decode_wait`, `JitRuntime`, `RUNTIME_{MACHINE,XREGS}_DISP`) and the three helpers: `jit_step_one` (per-instruction deopt → `Machine::step_instruction`; a `Step::Enter` returns the `ENTER` word with the target in `JitRuntime::enter`), `jit_tail_call` (`Machine::pop_frame_checked` for a linked local `call_last`) and `jit_set_position` (`Machine::set_current_code`, the block prologue's position store).
- `emit/` — the per-instruction emitters + usage-weighted regalloc; see `emit/CONTEXT.md`.

## Key facts
- **Inline dispatch**: the per-instruction `match instr` at `emit/mod.rs:445-716` (`emit_block`). Inlined groups: metadata no-ops, `Move`/`Swap`, `Test` (the word-level predicates **and** the boxed-term header kinds — see `emit/CONTEXT.md`), `SelectVal` (all-immediate), `SelectTupleArity`, cons readers (`GetList`/`GetHd`/`GetTl`), `GetTupleElement`, small-int `gc_bif` `'+'/'-'/'*'`, the 8 comparison BIFs, `Jump`, and local tail calls (`CallLast` via `emit_tail_exit`, `CallOnly` via `emit_exit`). Everything else is the `_` arm.
- **Deopt sequence**: `emit_step_and_continue` (`emit/mod.rs:779-792`) = `emit_helper_call` (`:747-760`, **spills first**) then `cmp rax, encode_target(func,pc+1)`; equal ⇒ `fill` and fall through in-block, else `jcc Ne, done`.
- **Driver entry** (`run_native_outcome` `:607-629`, `run_native_label_outcome` `:638-651`): `enter_machine` (`:527-541`: reset, seed `x[0..arity)`, `push_frame(0)`, `set_literals`, `build_label_map`) then `machine.enter_run(module, func)` then `drive`. `drive` (`:672-…`) latches `machine.trace_on`, loops on `code.enter`, and prices each deopt as one reduction.
- **Resume driver** (`run_compiled_resume_outcome`): the **no-reset** counterpart of `run_compiled_outcome` (it does NOT call `enter_machine`). It mirrors `Machine::resume_outcome` — `reset_reductions()` at slice start (so the resumed slice's `Machine::reductions()` matches the interpreter's), `machine.enter_run(module, func)`, `build_label_map`, then the shared `drive` entering the compiled block at the reported `(func, pc)` — so a slice suspended on the baseline tier resumes on it with interpreter-identical semantics. A non-entry module context (a suspension inside a cross-module callee) is served the same way the interpreter's resume serves it: `drive` reads the context from `Machine::current_context`, lands in it (`switch_into` re-binds the callee's module, label map and literal area and resolves its blocks through `NativeCode::module_blocks`), and re-enters the callee's own `(func, pc)`.
- **Cross-module driver** (`drive`): a `Step::Enter` (from its own interpreter step, or the `ENTER` word a block hands back) switches context through `switch_into` — `Machine::switch_context`, re-bind module/labels/literals, and the callee's blocks via `NativeCode::module_blocks` (the `BlockEntries` impl consults the per-thread `CodeCache`, compiling the callee on first sight) — then continues in the callee's compiled code, or on the per-instruction step when the tier has no blocks for it. `call_ext*` is never inlined, so the switch is a driver round-trip by construction. A **traced** run refuses native entry and hands the slice to `Machine::drive`.
- **Block prologue position store**: `emit_block` calls `jit_set_position(rt, func, block_start_pc)` after the six saved-register pushes and the `rt`-slot setup and before loading the cached x-registers, so a linked chain (which never returns to the driver) still advances `Machine::current_code` at block granularity.
- **Root-set contract**: block exits spill the 5 cached x-registers to `machine.x` (`spill` `emit/mod.rs:795-800`); the only two `call_r` sites (`emit/mod.rs:759`, `emit/link.rs:235`) spill first. Non-cached x-registers are addressed directly in `machine.x`.
- `FuncInfo` is NOT a metadata no-op (comment `emit/mod.rs:449-452`): it must deopt so a failed head clause raises `function_clause`.

## Constraints
- `#![allow(unsafe_code)]` on `mod.rs`/`runtime.rs` and `emit/*` (JIT zone); every `unsafe` block carries a `// SAFETY:` rationale.
- Never panic / never a wrong result: an unsupported shape is the typed `error::unsupported`.

## Test Strategy
`tests.rs` unit-tests the ABI encoding, the `#[repr(C)]` runtime layout, regalloc, the fast-path constants/arithmetic, and the deopt-path reduction charging / yield folding. Tier equivalence on real modules is in the crate's `tests/jit*.rs` (see `../CONTEXT.md`).

## Routing Table
- `emit/` → the per-instruction emitters (`mod.rs` dispatch + helpers, `move/test/select/aggregate/arith/compare/link/operands/regalloc.rs`); see its `CONTEXT.md`.
- `runtime.rs` → block ABI + `jit_step_one`/`jit_tail_call`; `tests.rs` → unit tests.
