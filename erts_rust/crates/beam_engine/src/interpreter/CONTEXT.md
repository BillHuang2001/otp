# `beam_engine` — Interpreter dispatch loop (`src/interpreter/`)

## Intent
The direct-threaded, one-instruction-at-a-time execution step of the Stage-1 engine: `Machine::step_instruction`
(`dispatch.rs`) matches on the frozen `beam_loader::Instr` variant and executes the **core arms** natively; every
variant it does not own is routed to `crate::families::dispatch` (`../families/mod.rs`).
Owns the per-run label/import index (`LabelMap`), BIF-argument marshalling + the BIF raise path, `Test` evaluation,
and the contract-A.8 trace emission.
This file is the crate-internal execution core: no BIF table lives here (all computation goes through
`ExecutionContext::call_bif`), and the module is split by responsibility (dispatch / bif / test / trace).
Ground truth (read-only, never modify): `../../../../../erts/emulator/beam/emu/*.tab` and `emu/beam_emu.c`.

## API Surface (crate-internal; all methods on `impl Machine`)
- `mod.rs` — module docs + `LabelMap` (dense or sparse label slots + the per-run `ImportTable`); `build_label_map`,
  `resolve`, `resolve_label`, `import_target` (the uncached fallback), `resolve_imports`.
- `dispatch.rs` — `run_loop`/`run_loop_with_map` (thin run entries over `Machine::drive`), `enter_run` (the per-run
  prologue that binds the literal area and records the entry call), **`Machine::step_instruction`** (the one-instruction
  dispatcher — also the JIT deopt target — which wraps the body `step_instruction_inner` with the uniform raise
  funnel, see below), `return_step`, `resolve_call_ext_apply` (the `erlang:apply/3` special case — C's
  `fixed_apply`→`apply()`), `resolve_call_ext_send` (the `erlang:send/2` / `'!'/2` off-heap-aware send),
  `resolve_external_entry` (the `call_ext*` cross-module seam), `call_ext_dispatch` (the single shared body of the
  three `call_ext*` arms — see below), `apply_import_target` + `ImportFrame` (the arm's own frame discipline over a
  resolved target). `ImportFrame` is `pub(crate)` and re-exported from `mod.rs` as
  `crate::interpreter::ImportFrame` (its variants inherit that visibility, so a sibling module can name
  `crate::interpreter::ImportFrame::Call`).
- `bif.rs` — `exec_bif` (`gc_bif`/`bif`/`bif0` operand marshalling + the fail-label/raise split), `call_import`
  (`call_ext*`), `raise_bif_error` (the catch-aware raise, shared with the families — `families::funs::apply_target`
  for an applied-target host BIF error, and `families::funs::call_fun` for the closure-apply fallback),
  `dispatch_wide`; `STACK_ARGS = 2` (arities ≤ it marshal through a fixed stack array, larger ones through the
  reusable `args_scratch`).
- `test.rs` — `Machine::eval_test` (all 25 `TestKind`s) + `eval_is_function2`/`eval_is_tagged_tuple`/`eval_test_arity`.
- `trace.rs` — the contract-A.8 `call`/`return_from`/`return_to` emission (gated on `Machine::trace_on`).
- `tests.rs` — the unit tests migrated from the former single-file `interpreter.rs`.

### The shared `call_ext*` entry point (`Machine::call_ext_dispatch`)
The three `call_ext*` forms differ **only** in their frame discipline, so their whole arm body lives in one method
whose signature is frozen for a sibling module (`crate::jit`) to drive:
```rust
pub(crate) fn call_ext_dispatch(
    &mut self, module: &Module, labels: &LabelMap, target: ImportRef, arity: u32,
    func: u32, pc: u32, frame: ImportFrame, ctx: &mut dyn ExecutionContext,
    seam: Option<Option<(u32, u32, u32)>>,
) -> StepResult;   // `StepResult` = `Result<Step, Box<BeamError>>` (the crate-internal hot step result)
```
Its resolution order is: `resolve_call_ext_apply` (the `erlang:apply/3` special case and
the cross-module call seam), then `resolve_call_ext_send`, then the host's cross-module seam, then the synchronous
`call_import` BIF route — the last one marshalled with the **instruction's own `arity` operand**, never the import
table's arity (the seam's own resolution uses the table's). The resolved `ApplyTarget` is then handed to
`apply_import_target` with the arm's `frame`, which is the only place the three forms diverge; every error is the
shared catch-aware `raise_bif_error`.
- `seam` is how a caller that has **already** consulted the host's cross-module seam for this import reports its
  answer: `None` = "resolve it here" (through `resolve_external_entry`), `Some(None)` = "the host declined" (skip
  straight to the BIF route, never re-asking), `Some(Some((context, func, pc)))` = the host's callee, entered
  verbatim. The three `step_instruction` arms pass `None`, so an interpreted `call_ext*` resolves through
  `resolve_external_entry` and is otherwise unchanged.
- Each arm keeps its own tracing (`trace_call_import`) **before** delegating, so contract A.8 emission order is
  unchanged.

## Instruction ownership
- **Core arms executed inline in `step_instruction`** (`dispatch.rs`): the metadata no-ops (`Label`, `Line`,
  `IntCodeEnd`, `ExecutableLine`, `DebugLine`, `NifStart`, `OnLoad`); `FuncInfo`; `Move`/`Swap`/`InitYregs`;
  `Test`; `SelectVal`; `Jump`; `GcBif`/`Bif`/`Bif0`; `Call`/`CallLast`/`CallOnly`/`CallExt`/`CallExtOnly`/`CallExtLast`
  (each `CallExt*` arm traces the callee's entry and then delegates its whole body to `Machine::call_ext_dispatch`,
  which runs the ordered resolution — the `erlang:apply/3` special case + the cross-module call seam, the
  `erlang:send/2` seam, the host's cross-module seam, the synchronous BIF route — and applies the form's
  `ImportFrame`);
  `Allocate`/`AllocateHeap`/`TestHeap`/`Deallocate`/`DeallocateReturn`/`Trim`; `Return`; `Generic`.
- **Everything else** falls to the catch-all `other => families::dispatch(...)` arm → the single owning family (tuples,
  records, lists, funs, exceptions, floats, maps, bitstrings, receive); an unclaimed or declined variant is the typed
  `unsupported("<ops.tab name>")`. Family ownership is disjoint (each variant has exactly one owner).

## Constraints / Notes for Agents
- Errors are `beam_common::Result` only — never a crate-local error enum, never a panic; an unsupported opcode/operand
  is the documented `error::unsupported`.
- **Reduction charge.** `Machine::drive` (`../machine/drive.rs` — the hot loop these run entries delegate to)
  charges **exactly one reduction per interpreted instruction, before the instruction's body**, and offers it to the
  host through the defaulted `ExecutionContext::reduce` hook; an `Err` from the hook ends the run **without applying
  the instruction**, leaving the machine parked at its `(func, pc)` for `Machine::resume_outcome` to apply exactly
  once. It is the only pre-emption point a **non-allocating** loop reaches (`ensure_heap` is never called by one),
  and `tests/reductions.rs` pins one charge per instruction.
- `step_instruction` records `Machine::current_code = (func, pc)` at entry (the exact position at every interpreted
  suspension, incl. a blocking `wait`); `Machine::drive` records the same position before it charges, so a
  pre-empted instruction stays that parked address. A natively-executed JIT block does not update it.
- **`step_instruction` wraps its body (`step_instruction_inner`) with the uniform raise funnel.** In the run's entry
  context (module context `0`) an uncaught `BeamError::Raise` gets its symbolic frames attached here via
  `Machine::with_uncaught_frames`; because this is the one body both the interpreter's dispatch loop and the native
  tier's per-instruction deopt helper (`../jit/baseline/runtime.rs::jit_step_one`) call, the two attach identical
  frames for the same raise. A raise in a cross-module callee context (non-zero) is left untouched here and framed by
  `Machine::drive`, which knows the run's true entry module.
- **`FuncInfo` is NOT a metadata no-op** — a failed head-pattern/guard jumps into it and it raises class `error` with the
  bare atom `function_clause`. Inlining it as a no-op would loop on the entry label. It raises carrying the failed
  clause's argument list through the shared `raise_function_clause_with_args` (see below).
- **The one `function_clause` raise has two entry points.** `raise_function_clause_with_args` (`dispatch.rs`,
  `pub(crate)`) is the definition — class `error`, the **bare atom** `function_clause` as the reason (the failed-clause
  reason: C's plain `FAIL(Fail)` sets no reason, so the `f,0` sentinel goes through `i_func_info_IaaI` →
  `handle_error`), resolved **only** from the running module's atom table (a module without that atom stays the typed
  `unsupported("<what>: no 'function_clause' atom in the module")`, never a substituted atom index). `what` names the
  raising site (`"func_info"`/`"test"`/`"select_val"`/`"maps"`). It additionally builds the failed clause's **real call
  argument list** — a proper list of the `argv` leading argument registers (`NIL` for a zero-arity call), read *after*
  the rooted reservation because `x` is in the frozen root set — and parks it on the machine as the one-shot
  `Machine::pending_function_clause_args` latch, which `Machine::throw_with_trace` promotes to the live
  `Machine::raise_function_clause_args` state the cooked-trace builders consume.
  **`raise_function_clause_no_fail`** (`pub(crate)`, re-exported as
  `crate::interpreter::raise_function_clause_no_fail`) is the **argument-free forwarder** to it (it passes no list, so
  the cooked item for the raising function falls back to its declared arity); the maps read path's absent-key `NO_FAIL`
  branch is its only consumer. `FuncInfo` (with `argv = arity.min(running_arity)`) and the `NO_FAIL`
  `Test`/`SelectVal` arms (with `argv = running_arity`) use the argument-carrying form.
  **The rule the cooked trace reports:** the failed function's argument list goes in the **frame-0** `A` slot (C's
  `save_stacktrace`'s `non_bif_stacktrace:` + `erts_build_mfa_item`), and **every outer frame keeps its declared
  arity** — so the oracle cooks `{Mod,Fun,[Arg,…],Location}` for the failing function and `{Mod,Fun,Arity,Location}`
  for each caller. A comparison `Test`/`SelectVal` uses `cmp` (`=:=`) vs `cmp_arith` (`==`).
- `gc_bif`/`bif` decode a boxed-term test only where the family/`predicates` do; the small-int `'+'`/`'-'`/`'*'` and the
  eight comparison BIFs have baseline-JIT inline paths (see `../jit/baseline/emit/`), everything else deopts to
  `jit_step_one` (`../jit/baseline/runtime.rs`), which calls `step_instruction`.

## Routing Table
- `dispatch.rs` → the dispatcher + run entries; `Machine::step_instruction` is the JIT helper's deopt target, and
  `Machine::call_ext_dispatch` + `ImportFrame` are the shared `call_ext*` body a tier drives.
- `bif.rs` → BIF operand marshalling + the fail-label/raise split (cross-ref `../context.rs` `ExecutionContext::call_bif`).
- `test.rs` → `TestKind` evaluation (raw type tests live in `../predicates.rs`).
- `mod.rs` → `LabelMap`/import resolution (the label+import index every run entry builds).
- Related semantics one level away: `../machine/` (frames, catch, run entries, location), `../families/` (per-family
  instruction bodies), `../jit/baseline/` (the native tier's inline set + `runtime.rs::jit_step_one`).
- `../CONTEXT.md` → the engine's binding execution semantics; `INSTRUCTION_SURFACE.md` → the exhaustive per-opcode
  surface; `TEST_NOTES.md` → the test/fixture enumeration.
