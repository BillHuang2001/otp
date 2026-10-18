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
  prologue that binds the literal area and records the entry call), **`Machine::step_instruction_with_entry`** (the
  one-instruction dispatcher every driver calls — the interpreter's `Machine::drive` **and** the native tiers'
  per-instruction deopt helper `../jit/baseline/runtime.rs::jit_step_one` — which wraps the body
  `step_instruction_inner` with the uniform raise funnel, see below), `Machine::step_instruction` (the
  single-module shorthand for it, `#[cfg(test)]`-callers only), `return_step`, `resolve_call_ext_apply` (the `erlang:apply/3` special case — C's
  `fixed_apply`→`apply()`), `resolve_call_ext_send` (the `erlang:send/2` / `'!'/2` off-heap-aware send),
  `resolve_external_entry` (the `call_ext*` cross-module seam), `call_ext_dispatch` (the single shared body of the
  three `call_ext*` arms — see below), `apply_import_target` + `ImportFrame` (the arm's own frame discipline over a
  resolved target). `ImportFrame` is `pub(crate)` and re-exported from `mod.rs` as
  `crate::interpreter::ImportFrame` (its variants inherit that visibility, so a sibling module can name
  `crate::interpreter::ImportFrame::Call`).
  **The dynamic execution census hooks live here.** `step_instruction_inner` looks the instruction up, calls
  `crate::exec_census::on_instruction(instr)` (**before** the effect), delegates the effect to
  `step_instruction_body(instr, …)` (the original `match instr` arms, byte-identical — the split exists so that an
  error propagated by `?` *inside* one arm, which returns from the body rather than falling out of the `match`, is
  still observed) and then calls `crate::exec_census::on_error(instr, err)` for an `Err` result. Both hooks are one
  thread-local `bool` load when the census is disabled.
- `bif.rs` — `exec_bif` (`gc_bif`/`bif`/`bif0` operand marshalling + the fail-label/raise split), `call_import`
  (`call_ext*`), `raise_bif_error` (the catch-aware raise, shared with the families — `families::funs::apply_target`
  for an applied-target host BIF error, and `families::funs::call_fun` for the closure-apply fallback),
  `raise_bif_error_with_head` (the same funnel with a synthesized callee head item) and the shared body `raise_error`;
  all three take `ctx`, because the reason **and** class atoms a landing installs resolve through the engine's live
  atom namespace (`../machine/catch.rs::namespace_atom` — `ctx.atoms(module)`, with the module snapshot as the
  fallback), so a `badarg`/`error` the runtime interned after the module was loaded still lands in its handler.
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
  `handle_error`). The reason atom **and** the `error` class atom resolve through the engine's live atom namespace
  (`../machine/catch.rs::namespace_atom`: `ctx.atoms(module)` — the host's `atoms` seam — with the running module's
  own table as the fallback), which is why every entry point takes `ctx`; that is the engine's equivalent of C's
  `am_function_clause`/`am_error` globals. A name **neither** source knows stays the typed
  `unsupported("<what>: no 'function_clause' atom in the module")`, never a substituted atom index; an `error` class
  the namespace cannot name is simply left uninstalled, so the raise lands only where the class could be installed.
  `what` names the raising site (`"func_info"`/`"test"`/`"select_val"`/`"maps"`). It additionally builds the failed
  clause's **real call argument list** — a proper list of the `argv` leading argument registers (`NIL` for a
  zero-arity call), read *after* the rooted reservation because `x` is in the frozen root set — and parks it on the
  machine as the one-shot `Machine::pending_function_clause_args` latch, which `Machine::throw_with_trace` promotes to
  the live `Machine::raise_function_clause_args` state the cooked-trace builders consume.
  **`failed_clause_arglist` clamps `argv` to `Machine::register_count()`** — this machine's own register file, not the
  `MAX_REGISTER` constant — so a machine narrowed with `Machine::with_max_register` can never read a slot that does not
  exist, and every register read is fallible (an out-of-range slot is the typed `bad_register`, never a substituted
  `NIL` in the argument list).
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
  `jit_step_one` (`../jit/baseline/runtime.rs`), which calls `step_instruction_with_entry`.

## BIF-argument marshalling: what the host actually receives (GC reality)
The host is handed a **copy** of the argument terms, never a borrow of the `x` file — and the copy lives in no root set.
- arity `<= STACK_ARGS` (2) → a **stack array** (`EMPTY_ARGS`, `bif.rs:51`), filled from the operands at `bif.rs:88-91` (`gc_bif`/`bif`) or from `x(0..nregs)` at `bif.rs:233-238` (`call_ext*`/local-call route).
- a wider arity → `Machine::args_scratch`, which `dispatch_wide` **takes out of the machine** (`std::mem::take`, `bif.rs:157`) and reinstalls *empty* at `bif.rs:167` — so during the dispatch the buffer is not reachable from `Machine` at all (`args_scratch` field doc: `machine/mod.rs:375-387`).
`Machine::with_roots` publishes only `x`/`stack`/`fr` (`machine/mod.rs:643-679`), so neither flavour is a GC root; and `Machine::dispatch_bif_call` takes **`&self`** (`bif.rs:291-305`) while `call_bif_in_machine` gets `&Machine` (`context.rs:413`), so a **collecting** host can neither relocate the argument words nor write relocated words back into `x`. `ExecutionContext::call_bif`/`call_bif_in_machine` state **no GC contract for `args`** (`context.rs:335`, `:413`) — the host rule is that a BIF dispatch must not move the calling process's heap.
Two further shapes to keep in mind when auditing this seam: `Instr::GcBif`/`Bif`/`Bif0`/`CallExt*` perform **no heap admission** of their own (the compiler's separate `test_heap`/`allocate_heap` is the only admission point), and `read_operand` of a **non-small** `Operand::Integer`/`Character` **allocates** via `ctx.alloc_words` (`operand.rs:69`, `:75` → `terms/number.rs:154`) *between* the reads of successive operands, i.e. inside the marshalling loops at `bif.rs:90`/`:112`, with the already-read terms unpublished. `trace_call` has the same shape: it fills the same taken-out scratch (`trace.rs:186-208`) and passes `&scratch` to `ctx.trace_event` **before** the `call_ext*` dispatch (`dispatch.rs:524-526`, `:541-543`, `:585-587`).

## Execution-position bookkeeping (exactness)
`Machine::drive` stores the position **before** pricing the instruction (`machine/drive.rs:169`), so a pre-empting `reduce` `Err` (`:181-184`) parks the machine on the NOT-yet-executed instruction; `step_instruction_with_entry` re-stores the same `(func, pc)` at `dispatch.rs:201` before the body (`:202`). The module **context** is switched after the step returns and before the next store (`drive.rs:216-226` → `machine/mod.rs:726`), so `current_code` and `self.context` agree at every charge point and every step entry.
A suspend is the one place the two positions differ: `step` reports `Step::Wait { func, pc }` = the **resume label** (`families/receive.rs:245-248`), while `current_code` still names the `wait`/`wait_timeout` instruction that suspended.

## Typed-error inventory (interpreter core)
Every typed error this core (and the `machine/` machinery it drives) can return is one of three classes; a real
`erl`-compiled `.beam` never hits classes 1 or 2 as a *divergence* — class 1 is malformed-bytecode robustness, class 2
is a capability gap owned by another node, and class 3 is the name-only refusal of a reason/class atom that no atom
source can name (the reason and class atoms themselves are resolved through the live namespace, so no
oracle-succeeds-but-we-bail site remains there).
- **Class 1 — malformed/hand-built-bytecode robustness (NOT reachable from valid `.beam`, so never a divergence).**
  These mirror C's own `ASSERT`/unchecked invariants; C would move `E`/index and hit UB, this core returns a typed
  error instead.
  - `run: pc out of range` (`dispatch.rs:232`) and `run: function index out of range` (`dispatch.rs:234`) — the
    dispatch loop's `func`/`pc` bounds; the loader bounds both for a real image.
  - Every `error::bad_register(n)` (`operand.rs:68/71/73/157/159/161/203/204`, `machine/registers.rs:201/223`,
    `machine/frames.rs:132/135/142/182`, `interpreter/bif.rs:239/261`, `dispatch.rs:804/805/806/933/934`) — the
    loader's decoder rejects a register index `>= MAX_REG = 1024` (`beam_loader/src/decode.rs:66`), so only a
    caller-side clamp (`Machine::with_max_register`) or a test reaches it.
  - `error::unknown_label(..)` → `BadArg` (`mod.rs:201/208/209`) and `call: import out of range` (`mod.rs:242/419`)
    — a label/import id the module does not define; the loader resolves these for a real image.
  - Frame guards `run: no current frame` (`frames.rs:163/175`), `pop_frame: no frame` (`:213`),
    `deallocate: no frame` / `deallocate: frame size mismatch` (`:225/228`), `trim: no frame` /
    `trim: n exceeds frame size` / `trim: stack underflow` (`:249/252/256`) — C's `deallocate` is
    `E = ADD_BYTE_OFFSET(E, N)` and `i_trim` is `E += N` (`emu/instrs.tab:66-69`, `:593-594`), both unchecked.
  - Catch guards `exceptions: stale catch frame` (`catch.rs:239/278`) and `exceptions: catch frame stack underflow`
    (`:282`), and `run: unknown module context` (`mod.rs:673`, `drive.rs:149/222`) — internal frame/context
    bookkeeping; only a bug in this crate or a malformed image reaches them.
  - `run_export: export not found` (`run.rs:116`), `process_info: function index out of range` (`location.rs:426`),
    the `operand: …` arms (`operand.rs:82/83/165/167/169/194/195`), `test: missing argument` (`test.rs:229`), the
    `*: arity operand` arms (`test.rs:147/166/210`) — all "the IR/operand is not shaped as this arm requires".
- **Class 2 — capability gaps owned OUTSIDE this node** (a typed error here is honest; the fix lives in the owner).
  - `Instr::Generic` sentinel → `Err(error::unsupported(op.name()))` (`generic.rs:44`): the loader emitted no typed
    variant. **0 corpus occurrences** (the whole-OTP census loads 1366 `.beam` / 2,888,257 instructions with zero
    residual `Generic`). Owner: `beam_loader` (add the typed variant).
  - `call: import module atom` / `call: import function atom` (`mod.rs:422/425`): the module's load-time atom table
    cannot name the import's atom. Owner: `beam_rt` (the shared live atom namespace) / the loader.
  - `operand: literal` (`operand.rs:82/165`): the index is past the module's `LitT`. Owner: `beam_loader`/`beam_codec`.
- **Class 3 — the exception reason/class atoms (resolved through the live atom namespace).**
  `function_clause_raise` (`dispatch.rs`) resolves the `function_clause` reason atom and installs the `error` class
  atom through `namespace_atom` (`ctx.atoms(module)` with the running module's own table as the fallback); the
  landing twin is `Machine::ensure_class_atoms` + `Machine::class_atom` (`machine/catch.rs`).
  C resolves these **globally**: `i_func_info_IaaI` sets `c_p->freason = EXC_FUNCTION_CLAUSE` and `goto handle_error`
  with no module lookup (`emu/beam_emu.c:578-583`), the reason comes from the global `error_atom[]` table
  (`beam_common.c:382-406`, `EXC_FUNCTION_CLAUSE` → `am_function_clause`), and the class atom is
  `exception_tag[GET_EXC_CLASS(...)]` (`beam_common.c:544`). The `atoms` seam is the engine's equivalent of those
  globals: `beam_rt` answers from its ONE live shared table (and seeds the `CORE_ATOMS` prefix with
  `function_clause`/`error`/`exit`/`throw`, `beam_rt/src/boot.rs`), while a bare embedder gets the module snapshot
  through the seam's default body.
  The residual is name-only: a name **neither** the live namespace nor the module snapshot knows stays the typed
  `unsupported("… no '<atom>' atom in the module")` / `exceptions: no atom for the '<class>' exception class …`,
  never a substituted index. A real module image (`fc.beam`, a failed head clause) whose atom table holds no
  `function_clause` still raises it under both runtimes.

**Headline:** every typed error here is malformed-bytecode robustness (class 1), an owner-outside-this-node
capability gap (class 2), or the name-only refusal of class 3, which the live-namespace seam answers for every host
that owns a shared table.
`error::blocked()` (the constructor is `../error.rs:75`; the fold into it is `../machine/run.rs:74`) is **not** a gap
— the blocking-aware entries report `RunOutcome::Wait`; the non-blocking `run_export`/`run_label` fold it
(Handoff #1, owner `beam_rt`).

## Routing Table
- `dispatch.rs` → the dispatcher + run entries; `Machine::step_instruction_with_entry` is the JIT helper's deopt
  target, and `Machine::call_ext_dispatch` + `ImportFrame` are the shared `call_ext*` body a tier drives.
- `bif.rs` → BIF operand marshalling + the fail-label/raise split (cross-ref `../context.rs` `ExecutionContext::call_bif`).
- `test.rs` → `TestKind` evaluation (raw type tests live in `../predicates.rs`).
- `mod.rs` → `LabelMap`/import resolution (the label+import index every run entry builds).
- Related semantics one level away: `../machine/` (frames, catch, run entries, location), `../families/` (per-family
  instruction bodies), `../jit/baseline/` (the native tier's inline set + `runtime.rs::jit_step_one`).
- `../CONTEXT.md` → the engine's binding execution semantics; `INSTRUCTION_SURFACE.md` → the exhaustive per-opcode
  surface; `TEST_NOTES.md` → the test/fixture enumeration.
