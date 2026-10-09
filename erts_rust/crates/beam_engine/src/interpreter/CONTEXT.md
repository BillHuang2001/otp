# `beam_engine::interpreter` — the direct-threaded dispatch loop

## Intent
The Stage-1 interpreter: one `match` arm per `beam_loader::Instr` variant, reproducing the C
emulator's instruction semantics (`../../../../erts/emulator/beam/emu/beam_emu.c` and `emu/*.tab`).
Failure is return-based: a `gc_bif`/`bif`/`call_ext` error with a live fail label branches (guard
failure), without one it raises (landing in a live `try`/`catch` frame, C's
`handle_error`→`next_catch`), otherwise a typed `BeamError`. Every unsupported opcode/operand is a
documented typed error, never a panic.

## API Surface (crate-internal)
- `mod.rs` — `LabelMap` (dense/sparse label index), `build_label_map`, `resolve`/`resolve_label`,
  the per-run import cache (`ImportTarget`/`ImportTable`, `LabelMap::import_target`) with the
  uncached fallback `import_target`. Module docs + label/import index only.
- `dispatch.rs` — `Machine::{run_loop, enter_run, step_instruction, return_step, run_loop_with_map,
  resolve_external_entry}`. `step_instruction` is the single-instruction body shared by the
  dispatch loop and the JIT deopt helper.
- `test.rs` — `Machine::eval_test` (the `Test`/`is_*` family; `#[inline]`) plus
  `eval_is_function2`/`eval_is_tagged_tuple`/`eval_test_arity` and the operand readers
  `test_arg`/`test_pair`.
- `bif.rs` — `Machine::{exec_bif, dispatch_wide, call_import, raise_bif_error}`; `STACK_ARGS = 2`
  stack-array marshalling with the `args_scratch` wide fallback.
- `trace.rs` — contract-A.8 emission (`trace_call_local`/`trace_call_import`/`trace_return`),
  infallible and event-skipping.
- `tests.rs` — `#[cfg(test)]` unit tests (label map, import cache, marshalling/fail contract).

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; no crate-local enum, no `unwrap`/`panic`/
  `unreachable!` on the production path (verified: only `tests.rs` uses them).
- Term construction/inspection is `beam_term` only; comparison is `beam_term::{cmp, cmp_arith}`.
- The `Test` match over `TestKind` is **exhaustive** (no `_ =>` fall-through), and an
  unclaimed instruction falls to `families::dispatch`, which returns the typed
  `unsupported("widened opcode not yet executed: <ops.tab name>")`. `Instr::Generic` →
  `error::unsupported(op.name())` (`crate::generic::exec`). Neither path returns a plausible value.
- `imports_owner` (`mod.rs`) keys the per-run import cache on the `Module`'s **address**; safe as
  used (the module outlives the map that borrows it in a run) but an address-identity assumption.

## Known Issues (verified against `../../../../erts/emulator/beam/`)
### 1. HIGH — arithmetic comparison tests are f64-approximated (`is_eq`/`is_ne` wrong branch)
`test.rs:62-69` (`IsEq` → `cmp_arith(...).is_eq()`, `IsNe` → `!cmp_arith(...).is_eq()`) delegates to
`beam_term::cmp_arith`, whose mixed int/float path converts BOTH operands via `number_as_f64`
(`beam_term/src/compare.rs:411-419`, `beam_term/src/number.rs:233`). C keeps an exact comparison
(`is_eq` = `CMP_EQ_ACTION`/`EQ_SPEC` → `erts_cmp_compound(X,Y,0,1)`, `emu/instrs.tab:1112`,
`erl_utils.h:127-147`). Impact: for an integer > 2^53 vs a float the WRONG branch is taken — e.g.
`9007199254740993 == 9007199254740992.0` is `false` in the oracle but the engine's `IsEq` test
**succeeds** (`(2^53+1) as f64 == 2^53.0`); likewise `18014398509481985 == 18014398509481984.0`.
Root cause is owned by `beam_term` (recorded there); the interpreter surfaces it through these arms.
### 2. MEDIUM — `is_boolean` degrades to `false` when the module lacks the `true`/`false` atoms
`test.rs:105-123` resolves `true`/`false` by name from `module.atom_table` and does `.unwrap_or(false)`,
whereas C compares against the **globally interned** `am_true`/`am_false`
(`emu/instrs.tab:933-937`). A real module `f(X) when is_boolean(X) -> ok; f(_) -> no.` has atom chunk
`{m,f,ok,no,module_info,erlang,get_module_info}` (verified with `erlc`/`beam_lib`) — **no** `true`/
`false`. Production `beam_rt` masks this (`runtime.rs:441-442` seeds `CORE_ATOMS`, which holds
`true`/`false`, before any load; `codeserver.rs:336` clones that table), but the engine's own test
host seeds `SEED_ATOMS` (`tests/support/atoms.rs:61-95`) which LACKS them, so a real module loaded
fresh under that host evaluates `is_boolean(true)` as **false** (wrong branch, no typed error). This
is the same "globally interned atom" pattern as `beam_engine` Handoff #2.
### 3. LOW — stale comment
`dispatch.rs:490-496` still claims "Each family is currently a stub returning `Ok(None)`"; the
families are implemented. Documentation-only.

## Notes for Agents
- `TestKind::TestArity` uses `is_tuple_arity` (an `ARITYVAL` header test) where C's `test_arity`
  dereferences `*tuple_val(Ptr)` with no boxed check (`emu/instrs.tab:1055-1058`); the compiler always
  precedes it with `is_tuple`, so the two agree on well-formed input (this is the documented
  Handoff #6 nuance, not a divergence). `is_tagged_tuple` is likewise equivalent (C's `BEAM_IS_TUPLE`
  is `is_boxed` in non-DEBUG, and the header compare subsumes it) — `emu/instrs.tab:1023-1030`.
- The rigorous, verified-correct parts: the raise/unwind and frame discipline (`dispatch.rs`
  `return_step`/`call_last`/`call_ext_last`), the `is_*` tag tests (all match the C predicates,
  including `is_pid`/`is_port`/`is_reference` = internal‖external and `is_binary` = whole bytes),
  `IsList` = `[]`‖cons, `IsNonemptyList` = cons (`emu/instrs.tab:881-886`), the exact comparisons
  (`IsEqExact`/`IsNeExact`/`SelectVal` via `cmp`, matching `select_literals`→`is_ne_exact`), and the
  BIF marshalling/fail-branch contract.
- Full engine detail (execution model, families, JIT, measured figures) lives in the parent
  `../CONTEXT.md`, `../INSTRUCTION_SURFACE.md`, `../TEST_NOTES.md`, `../MEASURED.md`.
