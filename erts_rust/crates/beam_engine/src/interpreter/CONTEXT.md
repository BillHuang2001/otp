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
  dispatcher — also the JIT deopt target), `return_step`, `resolve_external_entry` (the `call_ext*` cross-module seam).
- `bif.rs` — `exec_bif` (`gc_bif`/`bif`/`bif0` operand marshalling + the fail-label/raise split), `call_import`
  (`call_ext*`), `raise_bif_error` (the catch-aware raise, shared with the families), `dispatch_wide`;
  `STACK_ARGS = 2` (arities ≤ it marshal through a fixed stack array, larger ones through the reusable `args_scratch`).
- `test.rs` — `Machine::eval_test` (all 25 `TestKind`s) + `eval_is_function2`/`eval_is_tagged_tuple`/`eval_test_arity`.
- `trace.rs` — the contract-A.8 `call`/`return_from`/`return_to` emission (gated on `Machine::trace_on`).
- `tests.rs` — the unit tests migrated from the former single-file `interpreter.rs`.

## Instruction ownership
- **Core arms executed inline in `step_instruction`** (`dispatch.rs`): the metadata no-ops (`Label`, `Line`,
  `IntCodeEnd`, `ExecutableLine`, `DebugLine`, `NifStart`, `OnLoad`); `FuncInfo`; `Move`/`Swap`/`InitYregs`;
  `Test`; `SelectVal`; `Jump`; `GcBif`/`Bif`/`Bif0`; `Call`/`CallLast`/`CallOnly`/`CallExt`/`CallExtOnly`/`CallExtLast`;
  `Allocate`/`AllocateHeap`/`TestHeap`/`Deallocate`/`DeallocateReturn`/`Trim`; `Return`; `Generic`.
- **Everything else** falls to the catch-all `other => families::dispatch(...)` arm → the single owning family (tuples,
  records, lists, funs, exceptions, floats, maps, bitstrings, receive); an unclaimed or declined variant is the typed
  `unsupported("<ops.tab name>")`. Family ownership is disjoint (each variant has exactly one owner).

## Constraints / Notes for Agents
- Errors are `beam_common::Result` only — never a crate-local error enum, never a panic; an unsupported opcode/operand
  is the documented `error::unsupported`.
- **Reduction charge / cooperative pre-emption.** `Machine::drive` (`../machine/drive.rs` — the hot loop these run
  entries delegate to) charges **exactly one reduction per interpreted instruction** and offers it to the host once
  per instruction through `ExecutionContext::reduce_outcome`. A `ReduceOutcome::Yield` answer applies the instruction's
  `Step` **first**, then stops the loop at the *next* instruction and reports `RunOutcome::Yield { func, pc }`; the plain
  `reduce` hook stays honoured (the default `reduce_outcome` forwards to it, so a host overriding only `reduce` is
  byte-identical) and a **non-blocking** entry folds a `Yield` into the typed pre-emption error. Measured
  (`perf record -e instructions:u`): the single remaining `MinimalContext::reduce_outcome` vtable call is ~4.7 % of
  retired instructions and is **not** removable without changing the frozen reductions API (`tests/reductions.rs` pins
  one charge per instruction). The default body's `self.reduce(n)` is a *static* call — trait default bodies monomorphise
  per concrete `Self` — so it is not a second vtable dispatch.
- `step_instruction` records `Machine::current_code = (func, pc)` at entry (the exact position at every interpreted
  suspension, incl. a blocking `wait`); a natively-executed JIT block does not update it.
- **`FuncInfo` is NOT a metadata no-op** — a failed head-pattern/guard jumps into it and it raises class `error` with the
  bare atom `function_clause`. Inlining it as a no-op would loop on the entry label.
- `Test`/`SelectVal` raise on a `NO_FAIL` failure; a comparison `Test`/`SelectVal` uses `cmp` (`=:=`) vs `cmp_arith` (`==`).
- `gc_bif`/`bif` decode a boxed-term test only where the family/`predicates` do; the small-int `'+'`/`'-'`/`'*'` and the
  eight comparison BIFs have baseline-JIT inline paths (see `../jit/baseline/emit/`), everything else deopts to
  `jit_step_one` (`../jit/baseline/runtime.rs`), which calls `step_instruction`.

## Routing Table
- `dispatch.rs` → the dispatcher + run entries; `Machine::step_instruction` is the JIT helper's deopt target.
- `bif.rs` → BIF operand marshalling + the fail-label/raise split (cross-ref `../context.rs` `ExecutionContext::call_bif`).
- `test.rs` → `TestKind` evaluation (raw type tests live in `../predicates.rs`).
- `mod.rs` → `LabelMap`/import resolution (the label+import index every run entry builds).
- Related semantics one level away: `../machine/` (frames, catch, run entries, location), `../families/` (per-family
  instruction bodies), `../jit/baseline/` (the native tier's inline set + `runtime.rs::jit_step_one`).
- `../CONTEXT.md` → the engine's binding execution semantics; `INSTRUCTION_SURFACE.md` → the exhaustive per-opcode
  surface; `TEST_NOTES.md` → the test/fixture enumeration.
