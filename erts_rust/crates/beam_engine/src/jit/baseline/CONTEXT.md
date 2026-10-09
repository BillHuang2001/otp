# `beam_engine` native baseline JIT tier (`src/jit/baseline/`)

## Intent
The first `JitBackend` the engine builds: it lowers every basic block of a `beam_loader::Module`
into a real `extern "C"` x86-64 function and runs them through a driver loop, deopting any
instruction it does not inline to exactly one interpreter step. Correctness-first mechanism, not an
optimiser (DeepDive §F): every natively-executed result is interpreter-identical by construction.
Backend behind `Tier::BaselineJit`; `jit-beamasm` aliases this tier.

## API Surface
- `mod.rs` — `BaselineJit` (zero-sized `JitBackend`); `CompiledModule` (caller-owned artifact: `entry`, `block_count`, `cached_registers`, `highest_cached_register`, `highest_native_register`, `code_len`, `code_address_range`, `code_map`); the driver `run_compiled`/`run_compiled_outcome`/`run_compiled_label_outcome` and the shared `enter_machine`/`run_native`/`run_native_outcome`/`run_native_label_outcome`/`drive`; the `NativeCode` trait (`BlockEntries`).
- `runtime.rs` — the block ABI (`HALT`/`ERROR`/`WAIT_BIT`, `encode_target`/`decode_target`/`encode_wait`/`decode_wait`, `JitRuntime`, `RUNTIME_{MACHINE,XREGS}_DISP`) and the two helpers: `jit_step_one` (per-instruction deopt → `Machine::step_instruction`) and `jit_tail_call` (`Machine::pop_frame_checked` for a linked local `call_last`).
- `emit/` — the per-instruction emitters + usage-weighted regalloc; see `emit/CONTEXT.md`.

## Key facts
- **Inline dispatch**: the per-instruction `match instr` at `emit/mod.rs:445-716` (`emit_block`). Inlined groups: metadata no-ops, `Move`/`Swap`, word-level `Test`, `SelectVal` (all-immediate), `SelectTupleArity`, cons readers (`GetList`/`GetHd`/`GetTl`), `GetTupleElement`, small-int `gc_bif` `'+'/'-'/'*'`, the 8 comparison BIFs, `Jump`, and local tail calls (`CallLast` via `emit_tail_exit`, `CallOnly` via `emit_exit`). Everything else is the `_` arm.
- **Deopt sequence**: `emit_step_and_continue` (`emit/mod.rs:779-792`) = `emit_helper_call` (`:747-760`, **spills first**) then `cmp rax, encode_target(func,pc+1)`; equal ⇒ `fill` and fall through in-block, else `jcc Ne, done`.
- **Driver entry** (`run_native_outcome` `:607-629`, `run_native_label_outcome` `:638-651`): `enter_machine` (`:527-541`: reset, seed `x[0..arity)`, `push_frame(0)`, `set_literals`, `build_label_map`) then `machine.enter_run(module, func)` then `drive`. `drive` (`:672-…`) latches `machine.trace_on`, loops on `code.enter`, and prices each deopt as one reduction.
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
