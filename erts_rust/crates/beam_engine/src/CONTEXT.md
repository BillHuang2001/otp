# `beam_engine/src` — Engine Source Tree

## Intent
The implementation body of `beam_engine`: the typed-IR execution engine (direct-threaded interpreter,
module-context-aware driver, the nine instruction families, the term builders, the JIT tier seam).
This node owns `src/**` only; the crate's public API contract, the frozen seams and the exhaustive
execution semantics are documented one level up in `../CONTEXT.md` (read it first) and in
`../INSTRUCTION_SURFACE.md`.

## Cross-crate facts a `src/**` change must respect
- **`beam_engine::terms` is a PUBLIC module consumed cross-crate** by `beam_rt` (~24 files) and by
  `beam_engine/tests/**`. The **classic, unrooted** builders are the public names —
  `make_tuple(ctx, &[Eterm])`, `make_cons(ctx, head, tail)`, `make_list(ctx, &[Eterm])`,
  `badmatch_reason(ctx, module, value)`, `make_heap_binary(ctx, bytes)`, `make_float(ctx, v)`,
  `make_integer(ctx, v)`, `make_flatmap(ctx, pairs)`, `make_map(ctx, pairs, atoms)`,
  `make_fun(ctx, arity, uniq, index, free)` — they allocate through `ExecutionContext::alloc_words`.
  The **rooted** twins (`make_tuple_rooted`, `make_cons_rooted`, `make_list_rooted`,
  `badmatch_reason_rooted`, `make_heap_binary_rooted`, `make_float_rooted`, `make_integer_rooted`,
  `make_flatmap_rooted`, `make_map_rooted`, `make_fun_rooted`) take the `RootSet` and are what the
  engine's own instruction arms/families call. **Do not rename or re-signature the classic
  builders** — Rust has no overloading, so a signature change silently breaks `beam_rt` and the
  crate's own test suite (they must compile unchanged).
- **`machine::RunOutcome` is matched EXHAUSTIVELY cross-crate**: `beam_rt/src/boot/drive.rs` and
  `beam_rt/src/runner/run.rs`. It is exactly `{ Returned(Eterm), Wait { func, pc } }` — there is no
  yield/pre-emption variant. The reduction hook is the defaulted
  `ExecutionContext::reduce(n) -> Result<()>`, charged **once per interpreted instruction, before the
  instruction's effect**, by `Machine::drive` and observed through
  `Machine::reductions()`/`reset_reductions()`; a host pre-empts by returning its own typed error from
  `reduce`, which leaves the instruction unapplied and the machine parked at its `(func, pc)` so a
  `Machine::resume_outcome` there applies it exactly once.
- **`Step::Enter` crosses the module boundary on every tier.** A raise that lands in a catch frame
  belonging to another module context, a resolved cross-module `call_ext`, and a cross-module
  `return` onto a caller-context continuation all cross the module boundary as `Step::Enter`; the
  driver switches the context and re-binds the literal area there. The **interpreter** driver
  (`Machine::drive`) handles it directly; the **native baseline tier** handles it too — a block
  returns the `ENTER` block-ABI word, the driver switches context (reusing `Machine::switch_context` +
  the context registry + the literal rebind, exactly as `Machine::drive` does) and re-enters the
  callee's compiled blocks when the tier has them (else its per-instruction step). A seal-time
  cross-module *link* is unsound (each `CompiledModule` owns a separate executable mapping, `rel32`
  fixups are buffer-local, and the entry module is baked into `JitRuntime`), so the transfer stays a
  **driver round-trip by design**. Because cross-module landing works, a raise inside a cross-module
  callee (`gen:call`'s `exit(noproc)` inside `gen_server:call/3`'s `catch`, say) is caught and
  unwound exactly as the oracle does; the reason a caller then sees is whatever the callee's own
  wrapper built (a compound term, carried out in the `Raise` live-reason slot, never reshaped).
- **The in-machine BIF-dispatch door is latched by BOTH drivers.** `ExecutionContext`'s
  `wants_bif_in_machine()` probe (default `false`) is latched once per run into `Machine::bif_in_machine_on`
  — by `Machine::drive` (`machine/drive.rs`) **and** by the native baseline tier's own driver
  (`jit/baseline/mod.rs`; its deopt helper reaches the same funnel via `jit_step_one` →
  `step_instruction_with_entry`). Every BIF dispatch funnels through `Machine::dispatch_bif_call`
  (`interpreter/bif.rs`), which takes `ctx.call_bif_in_machine(self, entry_module, …)` when the gate is set
  and `ctx.call_bif(…)` otherwise; a host that opts in must be reached on **every** tier, so keep the two
  drivers latching the same answer (`tests/bif_in_machine.rs` is the tier-parity gate). The run's **entry
  module** (context `0`, the module `Machine::current_stacktrace`'s `module` parameter expects for a
  context-0 frame) is threaded from the driver through `step_instruction_with_entry` → `exec_bif` /
  `call_bif_named` / `dispatch_wide` / `families::dispatch` → `dispatch_bif_call`, and on the native tier
  through `JitRuntime.entry_module` (a field generated code never reads).
- `unsafe` only in the modules marked at the crate level (`terms`, `predicates`), each with its
  `// SAFETY:` rationale, plus the modules carrying a module-local `#![allow(unsafe_code)]` under
  the crate-wide deny (`context.rs` — the `MinimalContext` region heap is the crate's raw
  `beam_alloc::ReservedHeap` bump seam — `families/bitstrings/raw.rs`, `families/records/raw.rs`,
  `jit/emitter/*`, `jit/baseline/*`, feature-gated `jit/cranelift/*`). Errors are
  `beam_common::{Result, BeamError}` only, and unsupported cases stay typed errors.

## Routing Table
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `context.rs` → the `ExecutionContext` / `Mailbox` seams (`call_bif` + the defaulted in-machine twin
  `call_bif_in_machine` behind the `wants_bif_in_machine` probe; the resolver seams
  `resolve_external_call`/`resolve_local_fun`/`resolve_local_call`/`atom_name`), `RootSet`, `MinimalContext` (incl. its
  `ContextHeap` — a pointer-stable region heap over `beam_alloc::ReservedHeap`: ~4 MiB is reserved up
  front and **grown in place** as terms are built, so an allocation costs its payload only; on
  reservation exhaustion a fresh, larger region starts and **every earlier region is kept alive**, so a
  handed-out pointer stays valid and **unmoved** — `relocate` is deliberately never used. It does **not**
  override `alloc_words_rooted`, so it stays the test/embedding heap, not a collector.)
  (`CONTEXT.md` beside it: none — see `../CONTEXT.md`)
- `machine/` → `Machine` (registers + the `x_mut`/`fr_mut`/`stack_mut`/`with_roots`/`alloc_rooted`
  root-set hand-off, frames, catch/unwind, run entries, `RunOutcome`, module contexts, driver,
  reduction counter, position introspection) — `machine/CONTEXT.md`
- `interpreter/` → dispatch loop, BIF arm/raise, `TestKind`, A.8 trace emission — `interpreter/CONTEXT.md`
- `terms/` → the boxed-term builders/readers (classic + `*_rooted`), `map`, `fun`, `number`,
  `slack`, `sink`, rooted-allocation tests, `test_support` — `terms/CONTEXT.md`
- `families/` → the per-instruction-family seam (`tuples`, `lists`, `receive`, `funs`, `records`,
  `exceptions`, `floats`, `maps`, `bitstrings`), incl. each family's rooted tests
- `jit/` → the tier seam (`Tier`/`JitBackend`), the cross-run `CodeCache`, the emitter, the native
  baseline tier and the feature-gated Cranelift hot tier — `jit/CONTEXT.md`, `jit/baseline/CONTEXT.md`,
  `jit/baseline/emit/CONTEXT.md`, `jit/cranelift/CONTEXT.md`, `jit/bench/CONTEXT.md`
- Other top-level files: `step.rs` (`Step`), `operand.rs`, `predicates.rs`, `error.rs`
  (`unsupported`/`blocked`/…), `generic.rs` (the `Instr::Generic` sentinel)

## GC publication: which `src/**` engine→host calls can be intervened on
The engine hands a host its root set through **exactly three** doors: `Machine::with_roots`
(`machine/mod.rs:434`), `Machine::alloc_rooted` (`:464`, → `ExecutionContext::alloc_words_rooted`) and
`Machine::ensure_heap`/`allocate_heap` → `ExecutionContext::ensure_heap_full`
(`machine/frames.rs:289`). Every other engine→host call carries **no** root set, so a host that
*allocated* inside it would relocate the young heap against unpublished roots. Those calls, with the
state each holds in a Rust local across the call:
- `ExecutionContext::resolve_local_fun` (`families/funs/mod.rs:323`) — the `fun` Eterm (from `x(arity)`
  or the `call_fun2` operand, `:171-189`) and the `args` copy (`register_args`, `:275-280`).
  `terms::fun_free(fun)` (`terms/fun.rs:247`) then re-reads the term's **raw heap storage** *after* the
  call, so a move in the seam would dangle both.
- `ExecutionContext::apply_fun` (`:355`, the `Ok(None)` fallback) — the same `fun`/`args`; protection
  here is the host's `with_relocation_exempt`, not publication.
- `ExecutionContext::call_bif` (`interpreter/bif.rs`) — the marshalled arguments live in the
  `EMPTY_ARGS` stack array (`interpreter/bif.rs:50-53`, `:88-93`) or `Machine.args_scratch`
  (`machine/mod.rs:254`); **neither is in any `RootSet`**. The in-machine twin
  (`ExecutionContext::call_bif_in_machine`, reached through the single `Machine::dispatch_bif_call`
  funnel) carries the same arguments and the same constraint.

**Verified: no admission point can intervene on the `call_fun`/`call_fun2` path with the shipped host.**
`BudgetContext::resolve_local_fun` (`beam_rt/src/runner/budget.rs:220`) forwards **without** arming, and
`BudgetContext` arms only at `budget.rs:244/259/315` — each immediately followed by the delegate whose
first statement `mem::take`s the latch (`beam_rt/src/context/mod.rs:2655/2669/2693`) — so the latch can
never be armed-and-unconsumed across that seam call. `RuntimeContext::resolve_local_fun` → `local_fun_target`
(`:2630`/`:2232`) is `&self` and does no BEAM-heap allocation (only Rust-heap `Vec`s). The same holds for
the BIF-in-flight window: the `gc_bif` admission point runs *before* marshalling and
`BudgetContext::call_bif` (`budget.rs:177`) does not arm.

Consequences for a `src/**` change: the `Ok(Some)` copy loop needs **no** rooted builder — there is no
allocation to reserve, so `with_roots`/`alloc_rooted` do not apply (and cannot: the seam takes no
`RootSet`). If a host ever made the seam allocating, the sound fix is to **re-read** `fun` from
`x(arity)`/the operand and re-collect `args` after the seam call and before `fun_free`, never to
"publish" (there is no door to publish through).

**Engine-held live terms and the published root set.** The two heap-term raise latches
(`raise_function_clause_args`/`pending_function_clause_args`) are **parked into the published root set**
by `Machine::with_roots` for the duration of every hand-off and re-seated from their slots afterwards (a
`with_roots` fast path hands over an unmodified `RootSet` when neither is armed), so a collecting host
relocates them with no host-side change; the supplied-trace latches are `Vec<StackFrame>` metadata and
need no rooting. `Machine::pending_exception` (`machine/catch.rs`) is the remaining engine-held live
reason `Eterm`: it is rooted **host-side** by `beam_rt` via `Registers::extra_roots` (the write accessor a
collecting host needs, `Machine::pending_exception_mut`, exists). The other Rust-local vectors are
recorded in `../../beam_rt/M1_GAPS.md` and `../../beam_rt/src/context/CONTEXT.md`.

## See Also
- `../CONTEXT.md` → the crate-level FROZEN API surface, execution model, constraints and handoffs.
- `../INSTRUCTION_SURFACE.md` → per-opcode/per-family semantics and the full known-gaps list.
- `../TEST_NOTES.md`, `../MEASURED.md` → the test/fixture enumeration and the measured figures.
