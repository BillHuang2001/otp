# `beam_engine` — Machine state core (`src/machine/`)

## Intent
The process-state core of the Stage-1 engine: the register files, the Erlang stack, the call-frame and
exception catch-frame stacks, the module-context registry, the run entries, and `Machine::drive` — the
module-context-aware **dispatch loop that IS the interpreter's hot loop** (the `run_loop*` entries in
`../interpreter/dispatch.rs` are thin wrappers over it).

## API Surface (crate-internal; methods on `impl Machine`)
- `mod.rs` — `Machine` (x-register file, float-register (FR) file of `Eterm` term slots, Erlang stack,
  call frames, exception catch frames, literal area, module-context registry, latched A.8 trace gate);
  `MAX_REGISTER`/`MAX_FLOAT_REGISTER`; `reset`.
- `registers.rs` — x/FR access (`get_fr`/`set_fr`, bounds-checked → `bad_register`).
- `frames.rs` — `allocate*`/`deallocate*`/`trim`, the typed `Continuation` CP slot, and `Machine::ensure_heap`
  (the `ensure_heap_full` root-set admission point that hands over the whole `RootSet`).
- `catch.rs` — the `CatchFrame` stack + `throw`/`throw_error`/`throw_with_trace` (the one raise/unwind entry).
- `context.rs` — the `ModuleContext`/`Contexts` registry; `current_context`/`register_context`/`switch_context`/
  `module_context`.
- `location.rs` — execution-position introspection (`current_code`, `current_function`, `current_location`,
  `initial_call`, `current_stacktrace`).
- `run.rs` — `run_export`/`run_label` (non-blocking) and `run_export_outcome`/`run_label_outcome`/`resume_outcome`
  (blocking-aware); `RunOutcome` (`Returned` / `Wait`), re-exported at the crate root.
- `drive.rs` — **`Machine::drive`**: owns the `(func, pc)` position *and* the module context those indices belong
  to, folds every `Step` back into that position, charges **exactly one reduction per interpreted instruction**
  through `ExecutionContext::reduce` (the only pre-emption point a non-allocating loop reaches), and re-binds the
  literal area exactly on a `Step::Enter` into a different context.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no panic, no `unwrap()` on
  runtime data; an unsupported opcode/operand is the documented `error::unsupported`.
- `unsafe` only where the crate `CONTEXT.md` marks it, each block behind a `// SAFETY:` rationale.

## Notes for Agents
- **`Step::Enter` is the only way a run changes module context.** All three producers switch the context at the
  driver: a cross-module `call_ext*`, a cross-module `return` onto a `Continuation::Address` in another context, and
  a raise landing in another context's live catch frame (pinned by `drive.rs`'s tests
  `a_cross_module_return_switches_the_machine_back_into_the_caller_context` and
  `a_raise_landing_in_a_callers_catch_frame_switches_into_the_callers_context`).
- **`RunOutcome::Yield` is a suspend, not an error.** A host that exhausts its time slice reports it through
  `ExecutionContext::reduce_outcome`; the machine applies the current instruction and yields at the *next* one, so a
  `Yield` is resumed with `Machine::resume_outcome` exactly like a `Wait` address — no instruction is ever re-executed
  or re-charged. The non-blocking entries fold it into a typed pre-emption error instead.
- `reset` clears the register files, stacks, catch frames, contexts, pending exception and the trace gate, but **not**
  the `refc_binaries` arena.

## See Also
- `../interpreter/CONTEXT.md` → the per-instruction dispatcher (`step_instruction`, also the JIT deopt target).
- `../CONTEXT.md` → the engine's binding execution semantics; `../jit/CONTEXT.md` → the native tiers' block ABI.
