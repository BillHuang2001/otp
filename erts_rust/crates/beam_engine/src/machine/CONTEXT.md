# `beam_engine` — Machine state core (`src/machine/`)

## Intent
The process-state core of the Stage-1 engine: the register files, the Erlang stack, the call-frame and
exception catch-frame stacks, the module-context registry, the run entries, and `Machine::drive` — the
module-context-aware **dispatch loop that IS the interpreter's hot loop** (the `run_loop*` entries in
`../interpreter/dispatch.rs` are thin wrappers over it).

## API Surface (crate-internal; methods on `impl Machine`)
- `mod.rs` — `Machine` (x-register file, float-register (FR) file of `Eterm` term slots, Erlang stack,
  call frames, exception catch frames, literal area, module-context registry, latched A.8 trace gate,
  reduction counter); `MAX_REGISTER`/`MAX_FLOAT_REGISTER`; `reset`; the **root-set hand-off**
  (`stack_mut`, `with_roots` handing over the whole `RootSet`, and `alloc_rooted` — the convenience
  wrapper that calls `ExecutionContext::alloc_words_rooted`).
- `registers.rs` — x/FR access (`get_fr`/`set_fr`, bounds-checked → `bad_register`; `x_mut`/`fr_mut` as
  the single-view root-set accessors).
- `frames.rs` — `allocate*`/`deallocate*`/`trim`, the typed `Continuation` CP slot, and `Machine::ensure_heap`
  (the `ensure_heap_full` root-set admission point that hands over the whole `RootSet`).
- `catch.rs` — the `CatchFrame` stack + the raise/unwind entry (`throw`/`throw_error`/`throw_with_trace`) and the
  **uniform raise funnel**: `raise_stacktrace(entry_module: Option<&Module>, func, pc)` derives the symbolic `StackTrace` (raising function,
  then every live caller innermost first) and `with_uncaught_frames` attaches it to an uncaught `BeamError::Raise`
  (leaving a non-raise error, an already-framed raise and an **engine diagnostic** — a reason atom prefixed
  `unsupported_opcode: `, the `error::unsupported` sentinel — untouched). The funnel's first half runs at the
  `step_instruction` boundary (`../interpreter/dispatch.rs`), its **second half at the `drive` boundary** for a raise
  that surfaced inside a cross-module callee (`drive.rs`).
- `context.rs` — the `ModuleContext`/`Contexts` registry; `current_context`/`register_context`/`switch_context`/
  `module_context`.
- `location.rs` — execution-position introspection (`current_code`, `current_function`, `current_location`,
  `initial_call`, `current_stacktrace`).
- `run.rs` — `run_export`/`run_label` (non-blocking) and `run_export_outcome`/`run_label_outcome`/`resume_outcome`
  (blocking-aware); the **tier-selecting** entries `run_with_tier`/`run_export_outcome_with_tier`/
  `run_label_outcome_with_tier` (first entry) and `resume_outcome_with_tier` (**resume** — a slice that suspended on a
  native tier resumes on it: `Interpreter` → `resume_outcome`, `BaselineJit` → the cached no-reset baseline resume,
  `Cranelift` → a typed error); `RunOutcome` (`Returned` / `Wait`), re-exported at the crate root.
- `drive.rs` — **`Machine::drive`**: owns the `(func, pc)` position *and* the module context those indices belong
  to, folds every `Step` back into that position, charges **exactly one reduction per interpreted instruction**
  through the defaulted `ExecutionContext::reduce` (the only pre-emption point a non-allocating loop reaches;
  read back through `Machine::reductions()`/`reset_reductions()`), and re-binds the
  literal area exactly on a `Step::Enter` into a different context. It also runs the raise funnel's second half
  (`Machine::with_uncaught_frames`) over a raise that surfaced inside a cross-module callee — the driver is where
  the run's true entry module is known, so those caller frames are named against the right module.
- `tests.rs` — the cross-cutting end-to-end tests of the raise funnel: one synthetic module per raise kind
  (`throw`/`error`/`exit` `raw_raise`, a BIF error, `func_info`, a nested call, a cross-module callee), each run
  through a real run entry and asserting the propagated `Err(BeamError::Raise)` frames name the raising function and
  every live caller innermost first, plus the guard that a caught raise lands unchanged. `test_support.rs`
  (`cfg(test)` only) is the synthetic-module builder (`Fixture`) and cross-module seam context (`seam`) these share
  with `drive.rs`'s tests.

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
- `reset` clears the register files, stacks, catch frames, contexts, pending exception and the trace gate, but **not**
  the `refc_binaries` arena.

## See Also
- `../interpreter/CONTEXT.md` → the per-instruction dispatcher (`step_instruction`, also the JIT deopt target).
- `../CONTEXT.md` → the engine's binding execution semantics; `../jit/CONTEXT.md` → the native tiers' block ABI.
