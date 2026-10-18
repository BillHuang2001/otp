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
  `Machine::resume_outcome` there applies it exactly once. The **native baseline tier's** driver loop charges the
  same hook **once per driver-loop iteration** — one block entry, *before* `code.enter`, with the position
  published first — so a compiled loop is pre-emptible at block granularity (a natively linked tail-call chain
  stays inside one iteration and is priced once per round-trip; `tests/jit_reductions.rs`).
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
- **`ExecutionContext::atoms` is the live atom namespace, and it now serves the exception atoms too.** The
  defaulted `fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable` (default body:
  `&module.atom_table`) was introduced as the **comparison** namespace (guards/`select_val`/map-key
  equality order atoms by NAME, not by an id the module's load-time snapshot cannot name — `beam_term`'s
  `erts_cmp_atoms` falls back to the raw id for an unknown name). It is now also the namespace every
  **raise** resolves its reason/class atoms through, via the free function
  `machine::catch::namespace_atom(ctx, module, name)` — `ctx.atoms(module).lookup(name)`, then the module
  snapshot, then `None`. Consequence for a host: one shared live table answers both, so `beam_rt` overrides
  `atoms` once (in `RuntimeContext` and `BudgetContext`) and gets name-exact comparisons *and*
  nameable `badarg`/`function_clause`/`error`/`exit`/`throw` on the raise path. A name **neither** source
  knows is a typed error at every one of those sites, never a substituted atom index (an index is valid only
  against the table it was read with). The names reachable through it today: `function_clause` + the `error`
  class in the failed-clause raise, the reason + class of every BIF-raise landing (`interpreter/bif.rs`), and
  the three class atoms installed once per run by `Machine::ensure_class_atoms` (called from
  `families::dispatch`).
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
  `resolve_external_call`/`resolve_local_fun`/`resolve_local_call`/`atom_name`; and `atoms`, the live atom
  namespace used by comparison AND by the raise path's `machine::catch::namespace_atom`), `RootSet`, `MinimalContext` (incl. its
  `ContextHeap` — a pointer-stable region heap over `beam_alloc::ReservedHeap`: ~4 MiB is reserved up
  front and **grown in place** as terms are built, so an allocation costs its payload only; on
  reservation exhaustion a fresh, larger region starts and **every earlier region is kept alive**, so a
  handed-out pointer stays valid and **unmoved** — `relocate` is deliberately never used. It does **not**
  override `alloc_words_rooted`, so it stays the test/embedding heap, not a collector.)
  (`CONTEXT.md` beside it: none — see `../CONTEXT.md`)
- `machine/` → `Machine` (registers + the `x_mut`/`fr_mut`/`stack_mut`/`with_roots`/`alloc_rooted`
  root-set hand-off, frames, catch/unwind, run entries, `RunOutcome`, module contexts, driver,
  reduction counter, position introspection) — `machine/CONTEXT.md`
- `exec_census.rs` → the **dynamic** per-variant execution census (public, NOT `#[cfg(test)]`):
  thread-local enable flag + `Snapshot { executed, errors, first_error, total_* }`, the two funnel
  recorders `on_instruction`/`on_error`, `variant_name` (the one variant attribution, re-exported by
  `jit/baseline/counters.rs`) and `write_dump` (a TSV under the temp dir). Driven from
  `interpreter/dispatch.rs`'s `step_instruction_inner`; measurement-only, disabled by default
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

## Dynamic execution census (`exec_census`)
The **dynamic** counterpart of the static corpus census: per-`Instr`-variant counts of what actually *executes* at run time, keyed by `exec_census::variant_name` (a `Test` by its kind), plus per-variant typed-error counts with the exact `Display` text of each variant's first error.
It is a **public** module (`beam_engine::exec_census`, not `#[cfg(test)]`, following the `jit::bench` precedent) so the `tests/` integration drivers can call `enable`/`disable`/`set_enabled`/`is_enabled`/`reset`/`snapshot`/`write_dump`.
The recorders are the two `pub(crate)` funnel hooks in `interpreter/dispatch.rs` — `on_instruction` before the instruction's effect, `on_error` for an `Err` out of `step_instruction_body` — each one thread-local `bool` read when disabled; a counted run is byte-identical to an uncounted one (asserted in `exec_census.rs`'s tests).
Observability limits, honestly: an `Err` escaping the funnel is an error, so a raise folded into a `Step` by a live in-machine `try`/`catch` frame counts as **executed but not error**; and a natively-inlined baseline-JIT instruction never reaches the funnel, so `Tier::Interpreter` is the fully covered tier while a native tier reports only its deopted steps.
`write_dump(section)` writes `$TMPDIR/beam_engine_exec_census_<section>.tsv` (`variant`, `executed`, `errors`, `first_error`), the machine-readable form of a snapshot.
The instrument's own ranked view is `jit/baseline/census.rs`'s runtime **execution** census: `Tier::Interpreter` over the `jit::bench` workload set (13 workloads) = **31 720 executed instructions across 35 distinct variants, 0 typed errors**, topped by `GcBif` 6600, `Label` 4135, `Move` 3330, `Bif` 2565.
That workload set is the in-crate tier set, not the OTP corpus, so the corpus-scale dynamic ranking is a `tests/` driver's job.

## GC publication: which `src/**` engine→host calls can be intervened on
The engine hands a host its root set through **exactly three** doors: `Machine::with_roots`
(`machine/mod.rs:632`), `Machine::alloc_rooted` (`:690`, → `ExecutionContext::alloc_words_rooted`) and
`Machine::ensure_heap`/`allocate_heap` → `ExecutionContext::ensure_heap_full`
(`machine/frames.rs:283`).
Their call sites, exactly: `alloc_words_rooted` has **one** production caller — `terms/slack.rs:170`
(`terms::reserve`, hence `reserve_slack` and every `*_rooted` builder), reached **per built term**
(each `put_list`/`put_tuple2`/`make_fun3`/`put_map_assoc`/record/bitstring node), with
`RootSet::ALL_LIVE` at every family site.
The classic unrooted builders (`make_tuple`/`make_cons`/… in `terms/construct.rs`, `terms/number.rs`,
`terms/map.rs`, `terms/fun.rs`, `terms/sink.rs`) are the host/test API and reach
`ExecutionContext::alloc_words` directly, with neither a root set nor an admission point; **every in-crate
construction arm** uses a `*_rooted` twin instead. That rule — not a census of `alloc_words` callers — is what a
new engine-side build must follow.
`ensure_heap_full` is reached **only** from `Instr::TestHeap` (`interpreter/dispatch.rs:586`) and
`Instr::AllocateHeap` (`:581`) plus their baseline-JIT twins (`jit/baseline/runtime.rs:1407`/`:1443`),
so it is a **per-`test_heap`** door, never a per-allocation one; `Instr::Allocate` (`:569-574`) pushes
the frame with **no** admission.
`ExecutionContext::heap_available` has **no** call site anywhere in the crate (introspection only).
Every other engine→host call carries **no** root set, so a host that
*allocated* inside it would relocate the young heap against unpublished roots. Those calls, with the
state each holds in a Rust local across the call:
- `ExecutionContext::resolve_local_fun` (`families/funs/mod.rs` `fn call_fun`, the seam call at `:526`)
  — the `fun` Eterm (read by `FunSource::read`, `:513`, from `x(arity)` or the `call_fun2` operand) and
  the `args` copy (`fn register_args`, `:301`).
  `terms::fun_free(fun)` (`terms/fun.rs:247`) then re-reads the term's **raw heap storage** *after* the
  call, so a move in the seam would dangle both.
- `ExecutionContext::apply_fun` (`:558`, the `Ok(None)` fallback) — the same `fun`/`args`; protection
  here is the host's `with_relocation_exempt`, not publication.
- `ExecutionContext::call_bif` (`interpreter/bif.rs`) — the marshalled arguments live in the
  `EMPTY_ARGS` stack array (`interpreter/bif.rs:50-53`, `:88-93`) or `Machine.args_scratch`
  (`machine/mod.rs:385`); **neither is in any `RootSet`**. The in-machine twin
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

## Long files (role-justified, beyond the ~1000-line guideline)
Each file below is one cohesive unit — a seam module, a dispatch loop, or a comprehensive test
suite — and is kept whole; its size is a consequence of that single responsibility, not of
unrelated code accumulating:
- `context.rs` (~2175) — the entire runtime seam (`ExecutionContext` + `Mailbox` + `RootSet` + the
  `MinimalContext` region heap).
- `interpreter/dispatch.rs` (~1367) — the dispatch loop (`impl Machine`, `step_instruction` + every
  core arm).
- `interpreter/tests.rs` (~1090) — the dispatch-loop test suite.
- `families/funs/tests.rs` (~1845) — the comprehensive fun-opcode test suite (incl. the structured
  `{badfun,_}`/`{badarity,_}` target-shape cases).
- `families/records/mod.rs` (~1074) — the whole native-record instruction family.
- `families/exceptions/tests/cross_module.rs` (~1027) — the cross-module exception-containment suite.
- `families/maps/tests.rs` (~1002) — the map-family unit suite.

## See Also
- `../CONTEXT.md` → the crate-level FROZEN API surface, execution model, constraints and handoffs.
- `../INSTRUCTION_SURFACE.md` → per-opcode/per-family semantics and the full known-gaps list.
- `../TEST_NOTES.md`, `../MEASURED.md` → the test/fixture enumeration and the measured figures.
