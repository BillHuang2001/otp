# `beam_engine` — Interpreter dispatch loop (`src/interpreter/`)

## Intent
The direct-threaded, one-instruction-at-a-time execution step of the Stage-1 engine: `Machine::step_instruction`
(`dispatch.rs`) matches on the frozen `beam_loader::Instr` variant and executes the **core arms** natively; every
variant it does not own is routed to `crate::families::dispatch` (`../families/mod.rs`).
Owns the label/import index (`LabelMap`), BIF-argument marshalling + the BIF raise path, `Test` evaluation,
and the contract-A.8 trace emission.
This file is the crate-internal execution core: no BIF table lives here (all computation goes through
`ExecutionContext::call_bif`), and the module is split by responsibility (dispatch / bif / test / trace).
Ground truth (read-only, never modify): `../../../../../erts/emulator/beam/emu/*.tab` and `emu/beam_emu.c`.

## API Surface (crate-internal; all methods on `impl Machine`)
- `mod.rs` — module docs + `LabelMap` (dense or sparse label slots + the cached `ImportTable`);
  `build_label_map` (the whole-module build, then cached per machine per module by
  `Machine::label_map`/`label_map_arc` — `../machine/label_cache.rs`), `resolve`, `resolve_label`,
  `import_target` (the uncached fallback), `resolve_imports`.- `dispatch.rs` — `run_loop`/`run_loop_with_map` (thin run entries over `Machine::drive`), `enter_run` (the per-run
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
  `crate::exec_census::on_instruction(instr)` (**before** the effect) when the run's latched gate
  `Machine::census_on` is set, delegates the effect to
  `step_instruction_body(instr, …)` (the original `match instr` arms, byte-identical — the split exists so that an
  error propagated by `?` *inside* one arm, which returns from the body rather than falling out of the `match`, is
  still observed) and then calls `crate::exec_census::on_error(instr, err)` for an `Err` result, re-reading the
  same gate. The gate is a plain `bool` field the drivers latch once per run/resume entry
  (`Machine::drive` in `../machine/drive.rs` and the native tiers' driver in
  `../jit/baseline/mod.rs`; `Machine::reset` clears it, so a mid-run `enable()` takes effect at the
  run's next entry), so a disabled census costs **one load and a branch and zero thread-local
  accesses per instruction**. The gate is deliberately **not** cached in a local
  across `step_instruction_body`: the error arm is the only other reader, and holding it live across that call
  costs the hot frame a callee-saved register (measured: +4.5 % on `list-walk/1` vs −5.2 % with the re-read).
- `bif.rs` — `exec_bif` (`gc_bif`/`bif`/`bif0` operand marshalling + the fail-label/raise split), `call_import`
  (`call_ext*`), `raise_bif_error` (the catch-aware raise, shared with the families — `families::funs::apply_target`
  for an applied-target host BIF error, and `families::funs::call_fun` for the closure-apply fallback),
  `raise_bif_error_with_head` (the same funnel with a synthesized callee head item) and the shared body `raise_error`;
  all three take `ctx`, because the reason **and** class atoms a landing installs resolve through the engine's live
  atom namespace (`../machine/catch.rs::namespace_atom` — `ctx.atoms(module)`, with the module snapshot as the
  fallback), so a `badarg`/`error` the runtime interned after the module was loaded still lands in its handler.
  `dispatch_wide`; `STACK_ARGS = 2` (arities ≤ it marshal through a fixed stack array, larger ones through the
  reusable `args_scratch`).
  **Every marshalling helper returns `Result<BifOutcome>`**, not a bare value: `Machine::dispatch_bif_call`
  (`&self`) takes one of the two outcome-aware host doors, and a `BifOutcome::Suspend` at a re-execution-idempotent
  site becomes `Step::Suspend { func, pc }` — the calling instruction left completely un-applied — while the
  non-idempotent `erlang:apply/3` route (`families/funs/mod.rs`) reports the typed `unsupported` instead.
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
  `Test`; `SelectVal` (its pair decision is the ladder of `select.rs`, next section); `Jump`; `GcBif`/`Bif`/`Bif0`; `Call`/`CallLast`/`CallOnly`/`CallExt`/`CallExtOnly`/`CallExtLast`
  (each `CallExt*` arm traces the callee's entry and then delegates its whole body to `Machine::call_ext_dispatch`,
  which runs the ordered resolution — the `erlang:apply/3` special case + the cross-module call seam, the
  `erlang:send/2` seam, the host's cross-module seam, the synchronous BIF route — and applies the form's
  `ImportFrame`);
  `Allocate`/`AllocateHeap`/`TestHeap`/`Deallocate`/`DeallocateReturn`/`Trim`; `Return`; `Generic`.
- **Everything else** falls to the catch-all `other => families::dispatch(...)` arm → the single owning family (tuples,
  records, lists, funs, exceptions, floats, maps, bitstrings, receive); an unclaimed or declined variant is the typed
  `unsupported("<ops.tab name>")`. Family ownership is disjoint (each variant has exactly one owner).

## `select_val`: the per-pair ladder (`select.rs`)
`Instr::SelectVal` (`dispatch.rs`) reads the scrutinee, asks `select::first_match` for the first alternative whose
candidate is `=:=` to it (C's ordered scan, `../../erts/emulator/beam/emu/select_instrs.tab:130-153`, minus C's
loader-time sort — the IR keeps source order), then branches to that label, to `fail`, or — with `NO_FAIL` — into the
shared `raise_function_clause_with_args(.., "select_val", ..)`. **The per-pair decision is `select::pair_eq`, a
cost-ordered ladder**; `select.rs`'s module docs are its normative description. In brief, cheapest first:
1. **Unboxed immediate candidate.** `select::constant_word` answers the raw `Eterm` word of an
   `Operand::Integer`/`Character` inside `MIN_SMALL..=MAX_SMALL`, an `Operand::Atom`, or `Operand::Nil`, so the pair
   is one word compare against the scrutinee's own word and **no candidate term is built** (a small-int operand used
   to be materialized through `terms::make_integer_unrooted` on *every* pair). An out-of-range `Integer`/`Character`
   materializes as a bignum, so it is not an immediate constant and does not take this rung.
2. **Word identity.** For every other candidate (a register, a literal, a bignum constant) the value is read and its
   **word** compared: two identical words are `=:=` for every term kind, so a hit needs no ordering.
3. **Either side immediate.** The words differ (rung 2), and an immediate's primary tag (`11`) can never match a cons
   (`01`), boxed (`10`) or header (`00`) word, so this is a **definite** non-match — still with no `cmp`.
4. **Otherwise** the general `beam_term::cmp(..).is_eq()`, unchanged.

Rungs 1-3 are sound on the frozen layout facts the module tabulates and its tests pin: a word with low nibble `F` is a
small integer (so the compare *is* `=:=`, floats/bignums/atoms/binaries included), a word whose low 6 bits are `0xB`
is an atom, `0x3B` is `[]` and `0x1B` a catch term (no other class produces those bits, so `[]` never matches the
*atom* `'[]'`), and a cons/boxed word carries at most `TAG_LITERAL_PTR` above an 8-byte-aligned pointer. The unit
tests run the ladder against a `cmp`-only oracle over the adversarial battery (float vs int, two equal floats, two
equal bignums, a bignum candidate vs an out-of-range `Integer`, two equal heap binaries, the tag-collision table,
`NIL` vs `'[]'`, first-match-in-source-order).
`constant_word` is the **single** definition of "decidable by word identity": `../jit/baseline/emit/operands.rs` and
`../jit/cranelift/inline.rs` delegate their `constant_word` to it, so a tier's emitted word compare and the
interpreter's ladder cannot drift. The control flow (`fail`, `NO_FAIL`) is untouched, and a bad register operand
keeps `read_operand`'s typed `bad_register` — the ladder adds fast rungs, it weakens no error path.
**The test-only counter.** `select_answers()` -> `(decided_without_cmp, decided_by_cmp)` is a `#[cfg(test)]`
thread-local tally (the same idiom as `label_map_builds`), so a pair is counted exactly once and the production ladder
carries no counter at all. Consumers: `../bench/mod.rs` prints it per call
(`select_pairs_by_word_per_call`/`select_pairs_by_cmp_per_call`) and its
`every_bstr_select_val_pair_is_decided_without_a_general_compare` gate asserts `cmp == 0` on
`cowboy_bstr:to_lower/1`; `select.rs`'s `the_committed_fixture_is_decided_without_a_general_compare` asserts it on
`tests/fixtures/selval.beam` for **both** tiers (the interpreter answers every pair by word; the baseline tier inlines
the switch and never reaches the ladder). The fixture's *observable* half — both tiers against the live `erl`, plus a
zero-`alloc_words` claim on the immediate-only switches — is `tests/select_val.rs`.

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
  for each caller. A comparison `Test` uses `cmp` (`=:=`) vs `cmp_arith` (`==`); `SelectVal` always tests `=:=`
  (through the ladder below, which reproduces `cmp(..).is_eq()` exactly).
- `gc_bif`/`bif` decode a boxed-term test only where the family/`predicates` do; the small-int `'+'`/`'-'`/`'*'` and the
  eight comparison BIFs have baseline-JIT inline paths (see `../jit/baseline/emit/`), everything else deopts to
  `jit_step_one` (`../jit/baseline/runtime.rs`), which calls `step_instruction_with_entry`.

## BIF-argument marshalling: what the host actually receives (GC reality)
The host is handed a **copy** of the argument terms, never a borrow of the `x` file — and the copy lives in no root set.
- arity `<= STACK_ARGS` (2) → a **stack array** (`EMPTY_ARGS`, `bif.rs:51`), filled from the operands at `bif.rs:88-91` (`gc_bif`/`bif`) or from `x(0..nregs)` at `bif.rs:233-238` (`call_ext*`/local-call route).
- a wider arity → `Machine::args_scratch`, which `dispatch_wide` **takes out of the machine** (`std::mem::take`, `bif.rs:157`) and reinstalls *empty* at `bif.rs:167` — so during the dispatch the buffer is not reachable from `Machine` at all (`args_scratch` field doc: `machine/mod.rs:375-387`).
`Machine::with_roots` publishes only `x`/`stack`/`fr` (`machine/mod.rs:643-679`), so neither flavour is a GC root; and `Machine::dispatch_bif_call` takes **`&self`** and returns **`Result<BifOutcome>`** (`bif.rs:320-334` — one of the two outcome-aware doors, `call_bif_outcome`/`call_bif_in_machine_outcome`, never a bare `Eterm`) while `call_bif_in_machine` gets `&Machine` (`context.rs:597`), so a **collecting** host can neither relocate the argument words nor write relocated words back into `x`. `ExecutionContext::call_bif`/`call_bif_in_machine` state **no GC contract for `args`** (`context.rs:446`, `:597`); the invariant they rely on is the engine's, not the host's — the window is collection-free by construction (`dispatch_bif_call` is `&self`, both rooted doors need `&mut self` plus this machine's root slices, and the marshalling's only allocation is the non-collecting `alloc_words`), pinned by `tests/bif_args_window.rs`.
Two further shapes to keep in mind when auditing this seam: `Instr::GcBif`/`Bif`/`Bif0`/`CallExt*` perform **no heap admission** of their own (the compiler's separate `test_heap`/`allocate_heap` is the only admission point), and `read_operand` of a **non-small** `Operand::Integer`/`Character` **allocates** via `ctx.alloc_words` (`operand.rs:69`, `:75` → `terms/number.rs:154`) *between* the reads of successive operands, i.e. inside the marshalling loops at `bif.rs:90`/`:112`, with the already-read terms unpublished. `trace_call` has the same shape: it fills the same taken-out scratch (`trace.rs:186-208`) and passes `&scratch` to `ctx.trace_event` **before** the `call_ext*` dispatch (`dispatch.rs:524-526`, `:541-543`, `:585-587`).

## Execution-position bookkeeping (exactness)
`Machine::drive` stores the position **before** pricing the instruction (`machine/drive.rs:169`), so a pre-empting `reduce` `Err` (`:181-184`) parks the machine on the NOT-yet-executed instruction; `step_instruction_with_entry` re-stores the same `(func, pc)` at `dispatch.rs:201` before the body (`:202`). The module **context** is switched after the step returns and before the next store (`drive.rs:216-226` → `machine/mod.rs:726`), so `current_code` and `self.context` agree at every charge point and every step entry.
A suspend is the one place the two positions differ: `step` reports `Step::Wait { func, pc }` = the **resume label** (`families/receive.rs:245-248`), while `current_code` still names the `wait`/`wait_timeout` instruction that suspended.
`Step::Suspend { func, pc }` is the opposite case: the BIF instruction was **not** applied, so the reported address *is* the instruction (`current_code`), and the dispatch loop's arm for it returns `RunOutcome::Suspend` verbatim (`machine/drive.rs:250-252`) for the host's resume to re-execute.

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

## The local-call seam's memo (`Machine::local_callees`)
The local-call arm (`dispatch.rs:413-515`, seams at `:429`/`:475`/`:504`) does **not** call the host once per
call: both the callee identity **and** the host's verdict are memoized per `(module image address, function
index)` in `Machine::local_callees` (`../machine/local_callee.rs`, `LocalCallees`).
`Machine::local_call_seam` (`dispatch.rs:1149-1212`) reads it in three phases.
**Phase 1a, every local call:** `self.local_callees.lookup(module, callee)` — a fixed `CAP = 8` array held
as a compact 16-byte `keys` array (the only thing the scan reads) plus a parallel `verdicts: [Verdict; 8]`
byte array. **A hit reorders nothing** (the entry keeps its slot) and only a **miss** inserts — at the front,
dropping the oldest *insertion* — because the workloads this seam is priced on alternate two callees per
element (an MRU move-to-front memmoved up to eight entries on *every* call). The lookup returns a **byte**:
`Unnameable` (the memoized "no function entry / unresolvable name atom" — zero consultations) and `Declined`
(the host's own earlier `false`) both take the caller's unchanged in-machine jump, `Served` goes straight to
phase 2.
**Phase 1b, `Ask` only (once per distinct callee per run):** the host's `resolve_local_call` is asked and the
answer recorded (`record`), so the hot path makes **no** `dyn` call and never runs the host's `&str` predicate.
**Phase 2, served only:** the rare path re-derives the names from the `module` **parameter** through
`local_callee_identity` (`dispatch.rs:1354`), so the `&str`s borrow the module and not `self` and the BIF
dispatch keeps `&mut self` free; nothing is allocated on either path.
The memo's key is `(module as *const Module as usize, func)` — the address plus the function index — which is why
`Machine::reset` (`../machine/registers.rs`) clears it (the entries belong to the run that filled them, and every
module a run entered is kept alive by the context registry, so no address can be recycled while the memo holds it);
a resume does not reset.
The in-crate empirical anchor for the derivation's price is the sibling `LabelMap` cache: ~**5 ns** per BIF call
for resolving one import's two names against ~**0.5 ns** for the cached hit (`mod.rs:93-111`, `ImportTarget`,
which stores the two names `Box<str>` **owned** because the map is reached through a bare pointer inside
`JitRuntime` and cannot carry the module's lifetime) — the template `LocalCallees` follows.
**`Machine::local_call_seam` is the ONE definition of the seam both tiers drive** — the interpreter's three
local-call arms, `jit/baseline/call.rs::store_continuation` and `jit/baseline/runtime.rs::probe_local_call` — so the
memo inside it benefits the baseline tier with **zero** `../jit/baseline/**` edits.
**Why the verdict may be cached here at all.** The host predicate is a pure function of the
`(module, function, arity)` triple and ERTS charges **nothing** for an intra-module call, so a per-call
consultation is the deviation, not the memo. What can change an answer mid-run is an *installation*, so there
are exactly three invalidation doors: `erlang:load_nif/2` (cleared at the one BIF funnel
`Machine::dispatch_bif`, `bif.rs`, so an ordinary BIF pays one arity compare), `Machine::reset`, and the public
`Machine::invalidate_local_call_verdicts` a host with a stateful answer set calls itself.
Pinned by `../machine/tests.rs`'s seam tests (2 calls → 1 consultation, 1 derivation; each door forces a
re-consultation; an ordinary BIF dispatch does not; a served `call_last` pops its frame like the in-machine
route) and `../machine/local_callee.rs`'s unit tests (hits, eviction, `clear`, `invalidate_verdicts`,
`fill_count`, `ask_count`).
## Measured: interpreter hot-loop micro-optimizations
The wall-clock record for every interpreter hot-path lever (the section is `MEASURED.md`'s `## Measured (interpreter hot-loop micro-optimizations)`, moved here because this node owns those levers).
The `Machine`-side halves it cites (the `LabelMap` cache, the dirty-extent refill, the single-traversal `build_label_map`) are documented under `../machine/CONTEXT.md`.

## Measured (interpreter hot-loop micro-optimizations)
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test engine_bench -- --nocapture` (and `--test bif_cost` / `--test jit_bench -- --ignored`); BEFORE/AFTER are min-of-N interleaved runs — the host is heavily oversubscribed, so read them as ranges.
- **By-value step result** — `step_instruction`/`step_instruction_inner` return `Result<Step, Box<BeamError>>` (`src/interpreter/dispatch.rs`: a ~72 B `BeamError` becomes a 16 B register-returned value, the `Box` allocated only on the raise path), which improved every arithmetic/control row (`dispatch/noop` 5.5→3.2, `gc_bif '+'/2` 21.4→15.1, `tail-loop/2` 11.2→8.4, `list-walk/1` 10.7→9.5 ns/instr) while the allocation-dominated `jit_bench` rows (`map/2`/`aggr/2`/`build/0`) read ~1-7 % the *worse* (code layout on allocation-heavy paths).
- **Latched reduce gate** — `Machine::reduce_on` + the defaulted `ExecutionContext::wants_reduce` (default the conservative `true`; only a host whose `reduce` is the default no-op may return `false`): ~3-7 % on `ctrl/2`/`branch-loop/2`/`list-walk/1`/`tailcall:even/1`, ~12 % on `move/2`; `Machine::reductions()` is unchanged.
- **Latched census gate** — the census (`src/exec_census.rs`) is gated by a **`Machine` field**, not a per-instruction thread-local: each driver copies `exec_census::is_enabled()` into `Machine::census_on` once at its run/resume entry (`machine/drive.rs`, `jit/baseline/mod.rs`), `reset` clears it, and the per-instruction funnel (`interpreter/dispatch.rs::step_instruction_inner`) tests the field — a **disabled** census costs one load and a branch, zero thread-local accesses per instruction. The latch is deliberately **not** held in a local across `step_instruction_body` (the cold error arm is its only other reader, and keeping it live spends a callee-saved register per instruction): that variant reads `list-walk/1` **+4.5 %** and `setup/empty` **+21.8 %** (8/8), while re-reading the field on the error arm — the shipped shape — reads `list-walk/1` **0.948** (8/8). Counts are unchanged (census **37 159** instructions / **103** names / `total_errors = 4`; proofs 55/55).
  Paired same-binary A/B, 10 rounds, 1-min `/proc/loadavg` 4.2–7.0, rebuild control byte-identical (sha256 `a0deeec3…`), `NEW/OLD` < 1 ⇒ cheaper: `dispatch/noop` **0.943** (10/10), `move/2` **0.932** (10/10), `test/2` **0.933** (9/10), `setup/empty` **0.984** (9/10), `foreign_bstr_to_lower` interpreter **0.979** (7/10; second block 0.980, 6/8). That workload's **baseline** row (**1.012**) means nothing there: the tier deopts 0 steps/call on this shape, so the funnel is never reached. Pin: `src/exec_census.rs::the_census_gate_is_latched_at_the_run_entry`.
- **Per-`Machine` `LabelMap` cache** (`src/machine/label_cache.rs`) — `Machine::label_map`/`label_map_arc` answer from a **bounded LRU** of up to `LABEL_MAP_CAPACITY` (32) entries per machine, keyed on the module's load-time `Digest` + its `AtomSnapshot` + the image address, **content-keyed so it survives `reset`**: a hit bumps that entry's monotone recency stamp **in place** (the `Vec` is never reordered), a miss inserts as most-recently-used and evicts the entry with the smallest stamp; the `Arc` route stores the module handle so a recycled address cannot alias, and a synthetic (all-zero-checksum) module has no content identity and is never cached. The bound is what a **nested run** needs: `reset` clears the module-context registry, so every in-machine callee is re-registered through the `Arc` route, and an entry module alternating with its callees no longer rebuilds a whole-module map per callee per run. Structural counter (`interpreter::label_map_builds()`, pinned by `src/machine/label_cache_tests.rs`, `src/machine/run.rs`'s build-count tests and `src/jit/baseline/tests.rs`'s artifact-count test): **one build on a module's first run through a machine and 0 on every later run while it stays resident**. Wall clock and its never-cached-route regression: `## Measured (per-`Machine` label-map cache)`, the only place its ratio is quoted.
- **Dirty-extent register refill** — `Machine::reset` refills `max(mark, dirty)` words **per file** (`used_extent`: the larger of the `set_x`/`set_fr` high-water mark and the writer-reported dirty extent, clamped to the file), never the 2048 words of both. The dirty extent is what a writer that cannot report its slots declares: `x_mut`/`fr_mut` (the whole file), `with_roots` (the prefix it hands over) and the native driver (`CompiledModule::native_write_extent`, noted **before** the block runs; generated code never stores into `fr`, so the native path declares no FR extent). `refilled_words()` asserts **3**, **9** (`x7` + `fr2`), **1 + 41** (a native `x0` artifact extent plus an `fr40` `set_fr` write) and **901** (a deep native write to `x(900)`), against **2048**. Its own paired A/B does not move the `engine_bench` `setup/empty` floor (138.9 / 138.6 ns/run) — that floor is the interpreter entry, already mark-bounded — so the saving is on the allocation- and native-entry paths; the floor's current reading (**159.7–163.6 ns/run**) is the label-map cache's never-cached route instead.
- **Single-traversal `build_label_map`** — one pass over the label tables (the entry buffer sized from the `Code` header's exact `label_count`, clamped), so a build costs `lists.beam` **6.5 µs**, `string.beam` **6.0 µs**, `tailcall.beam` **193 ns**; it happens once per machine per module (the cache bullet above), never per run.
- **Declined on measurement (not carried):** devirtualizing the `bif`/`gc_bif` `&mut dyn ExecutionContext::call_bif` crossing — ceiling ~0.01-0.03 ns/call (the ~2.4 ns `bif_cost` attributes is the *host's own* hook body); memoizing the `LabelMap` on a **content** key — every sound content key (a full `Function::labels` traversal, or the `jit::cache` fingerprint at ~141 µs for a 2048-instruction module) costs at least the single-pass build it replaces, so the cache keys on **identity** (O(1)); a per-`func` cache of the resolved slice — it **costs** 20-40 % on the tight dispatch rows, since `module.functions.get(func)` is already a bounds check + one `imul` + an L1 load.
- **`select_val` per-pair fast ladder** (`src/interpreter/select.rs`) — `Instr::SelectVal` decides each pair on a cost-ordered ladder (a fixed word for an `Integer`/`Character`/`Atom`/`NIL` operand with no candidate term built; raw-word identity; an immediate ⇒ *definite* non-match; the general `cmp` last, with `ctx.atoms(module)` taken only inside that rung), so a pure-Erlang `select_val` dispatch pays no structural compare per pair — `cowboy_bstr:char_to_lower/1`'s 27-clause switch is **721 pairs per call**, every one decided by word. `constant_word` is the **single** definition shared by `jit/baseline/emit/operands.rs` and `jit/cranelift/inline.rs`, so the tiers cannot drift.
  Paired same-binary A/B: **one** test binary shape (`--lib --ignored foreign_bstr_to_lower_decomposition`) built from **two** trees and run alternately, 7 rounds, `BENCH_TIER=both BENCH_ITERS=20000` (20 000 measured + 2 000 warm-up calls per leg), `taskset -c 3`, one process at a time; `OLD` = commit `43f44f4991` (pre-ladder), `NEW` = the current tree. 1-min `/proc/loadavg` at both ends of every leg: **58.94–65.18** on a 14-CPU shared host. ns/call (min / median):
  | round | `OLD` interp | `NEW` interp | `NEW/OLD` | `OLD` baseline | `NEW` baseline | `NEW/OLD` |
  |---|---|---|---|---|---|---|
  | 1 | 26 622.5 / 27 635.4 | 20 550.9 / 20 816.1 | 0.772 | 16 994.9 / 17 729.2 | 16 657.5 / 17 134.7 | 0.980 |
  | 2 | 27 243.3 / 28 433.2 | 20 764.3 / 21 259.9 | 0.762 | 17 424.0 / 17 862.5 | 16 509.8 / 17 078.1 | 0.948 |
  | 3 | 28 027.2 / 29 464.0 | 21 028.2 / 21 822.9 | 0.750 | 16 994.6 / 17 374.5 | 17 128.6 / 17 588.4 | 1.008 |
  | 4 | 28 525.2 / 29 427.7 | 20 184.6 / 21 506.4 | 0.708 | 18 177.1 / 18 401.4 | 15 923.2 / 16 497.3 | 0.876 |
  | 5 | 27 960.4 / 28 947.2 | 23 626.3 / 23 984.3 | 0.845 | 18 576.2 / 18 799.6 | 18 252.0 / 18 525.5 | 0.983 |
  | 6 | 30 738.4 / 30 884.8 | 22 697.6 / 23 438.9 | 0.738 | 18 403.9 / 19 445.5 | 18 738.0 / 19 308.4 | 1.018 |
  | 7 | 31 045.7 / 31 281.7 | 22 965.9 / 23 829.2 | 0.740 | 19 378.5 / 19 934.0 | 18 170.5 / 19 148.1 | 0.938 |

  `NEW/OLD` is the per-round min/min ratio. Interpreter **0.708–0.845×**, all seven rounds below 1 (**sign test 7/7**), median of the per-round ratios **0.750×**, median-of-mins 28 027 / 21 028 ns/call, median-of-medians 29 428 / 21 823 ns/call. Baseline tier **0.876–1.018×** (straddles 1, sign 4/7) — the **noise control**: that tier inlines the switch into its emitted block and never reaches the ladder. Both sides ran every round; no row withheld.
  **Deterministic counter** (exact over 22 000 calls): interpreter **721.00** pairs/call decided by a ladder rung, **0.00** by `cmp`; baseline tier `0.00` / `0.00`. Pins: `select.rs`'s seven ladder tests (an adversarial battery against a `cmp`-only oracle), `select.rs::the_committed_fixture_is_decided_without_a_general_compare`, `src/bench/mod.rs::every_bstr_select_val_pair_is_decided_without_a_general_compare`, `tests/select_val.rs` (27 `CASES` on `tests/fixtures/selval.beam`: tier equality, a batched live-`erl` oracle matrix, `an_immediate_only_switch_allocates_nothing_on_either_tier`, `the_allocation_counter_sees_a_real_allocation`).

## Routing Table
- `dispatch.rs` → the dispatcher + run entries; `Machine::step_instruction_with_entry` is the JIT helper's deopt
  target, and `Machine::call_ext_dispatch` + `ImportFrame` are the shared `call_ext*` body a tier drives.
- `bif.rs` → BIF operand marshalling + the fail-label/raise split (cross-ref `../context.rs` `ExecutionContext::call_bif`).
- `test.rs` → `TestKind` evaluation (raw type tests live in `../predicates.rs`).
- `select.rs` → the `select_val` **pair ladder** (`first_match`/`pair_eq`), `constant_word` (the single definition
  both JIT tiers' `select_val` plans delegate to) and the `#[cfg(test)]` pair tally (`reset_select_answers`/
  `select_answers`).
- `mod.rs` → `LabelMap`/import resolution (the label+import index a machine builds once per module and
  then serves from `../machine/label_cache.rs`).
- Related semantics one level away: `../machine/` (frames, catch, run entries, location), `../families/` (per-family
  instruction bodies), `../jit/baseline/` (the native tier's inline set + `runtime.rs::jit_step_one`).
- `../CONTEXT.md` → the engine's binding execution semantics; `INSTRUCTION_SURFACE.md` → the exhaustive per-opcode
  surface; `TEST_NOTES.md` → the test/fixture enumeration.

## The `call_ext*` / `return` / raise context transfers (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **The `call_ext*` / `return` / raise context transfers.** Each `CallExt`/`CallExtOnly`/`CallExtLast` arm first calls `resolve_call_ext_apply` (`dispatch.rs:689`), the `erlang:apply/3` special case: for an `erlang:apply/3` import (`import_arity == 3`; `erlang:apply/2` is `apply(Fun, Args)` and is not special-cased) it reproduces C's `fixed_apply` -> `apply()` register decode (`x(0)=Module`, `x(1)=Function`, `x(2)=Args`, the nested `apply of apply/3` unwrap, the argument-list walk -- `MAX_ARG` -> `error:system_limit`, a non-proper tail -> the catchable `badarg`) and hands the outcome to `families::funs::apply_target`; any other import returns `None`. Otherwise the arm resolves the import through `resolve_external_entry` (the per-run `LabelMap::import_target` for the `(module, function, arity)`, then `ctx.resolve_external_call`; a `Some` registers the callee context and resolves the entry label against the callee's own map). On `Some` they use the LOCAL discipline and report `Step::Enter`: `CallExt` = `Instr::Call` (stores `Continuation::Address{func, pc+1, context: current_context()}` in the current frame, i.e. the caller's context), `CallExtOnly` = `CallOnly` (no continuation change), `CallExtLast` = `CallLast` (the target is resolved **before** `pop_frame_checked(deallocate)`, and the arm applies the same `ImportFrame` discipline to a resolved `apply/3` target via `apply_import_target`). On `None` they are byte-identical to a plain `call_bif` dispatch. A **host BIF error** from the resolved target -- on the synchronous `call_bif` route or the `apply/3` route -- is routed through `Machine::raise_bif_error` (`ApplyTarget::Raised`), landing in the caller's live `try`/`catch`. `Instr::Return` emits `Step::Enter` when the target context differs; `throw_with_trace` emits `Step::Enter` when a live catch frame lives in another context; `ApplyLast` follows its (popped) frame's continuation through `Machine::continuation_step` and emits `Step::Enter` when that continuation's context differs from the running module's.

## The `resolve_local_call` seam: host contract and the `prim_tty`/`zlib` motivation (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
13. **A built-in / static NIF library is served through the `resolve_local_call` seam -- ADOPTED (owner `beam_rt`).** The three **local** call arms (`Instr::Call`/`CallOnly`/`CallLast`) now consult `ExecutionContext::resolve_local_call` before returning their in-machine `Step::Goto`, and the native baseline tier probes the same seam at run time in `jit_local_call`; on a hit the engine marshals `x(0..arity)` and dispatches the callee through `call_bif` under the arm's own frame discipline (see the seam bullet in the API-Surface section). The motivation is ERTS's split module shape: the OTP compiler emits a **local** `call`/`call_only`/`call_last` for a **non-exported** same-module function but a self-qualified `call_ext` for an **exported** one, so `lib/kernel/src/prim_tty.erl`'s Erlang wrappers `init/1`/`reader_read/2`/`writer_loop/2` chain **locally** into the non-exported NIF stubs `tty_create/1`/`setlocale/1`/`setupterm_nif/0`/`read_nif/3`/`write_nif/2`/... (bodies `erlang:nif_error(undef)`), while only the exported `isatty/1` is reached by the caller's `call_ext`. So such a module's own Erlang runs in-machine and the host intercepts only the NIF stubs. **Correctness constraint:** `Ok(false)`, an absent resolver, and an un-nameable/missing function entry all leave the arm byte-identical to the pre-seam path (same continuation store, same `pop_frame_checked`, same trace emission, same `Step::Goto`), and the interpreter and native baseline tiers are pinned equal on both the served and the declined route (`tests/local_call_seam.rs`, incl. the eager-layout probe and the `CallLast` frame pop). The host side is landed too: `beam_rt/src/context/execution.rs:83` (`resolve_local_call`) answers over **two** key sets -- `crate::native::prim_tty::serves_local_call` for `prim_tty`'s non-exported stubs, then `beam_bifs::serves_local_call` (a `module == "zlib"` equality plus its 19 `(name, arity)` rows) for `zlib`'s -- forwarded by `beam_rt/src/runner/budget.rs:542`; no engine work is outstanding. **The same keys also arrive as a fun, not a `call`:** `erts/preloaded/src/zlib.erl` keeps the stub in a `#zlib_opts{}` record and applies it (`fun deflate_nif/4`), so `deflate/3`/`inflate/3`/`safeInflate/2` reach it through a local `make_fun3` closure -- `beam_rt/src/context/apply.rs`'s `served_local_key` makes `local_fun_target` decline it (`Ok(None)`) and `apply_non_local_fun` dispatch it through `call_bif` over the same predicate.

## Hot-path inlining: which helpers carry which attribute, and the structural residual (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **Hot-path inlining is measurement-driven, never blanket.** An out-of-line helper returning `Result<_, BeamError>` (a 72-byte `Err`) forces its `Ok` through memory, so the interpreter's *small* hot helpers are `#[inline(always)]` — `read_operand`/`write_operand` (`src/operand.rs`), `test_arg` (`src/interpreter/test.rs`), `set_y` (`src/machine/frames.rs`) — while `step_instruction` and `eval_test` carry plain `#[inline]`. The same attribute is a **measured regression on large helpers**: `#[inline(always)]` on `exec_bif`/`call_import` costs ~20%, and caching the resolved `functions[func].code` slice across the dispatch loop costs ~19% (the extra live range across `&mut self` calls and the code bloat outweigh the saved `Vec::get`), so neither is applied. The residual per-instruction cost is structural, not a missed hint: the 64-byte `Instr` discriminant `match` in `step_instruction` (a mispredicted indirect jump), the `Result<Step>` loop plumbing, the double `Vec` bounds check (`functions.get`/`code.get`), and the arity-2 `gc_bif` marshal (the largest single leaf on the list walk).

## Contract A.8 trace emission: the exact points (from `../../CONTEXT.md`)
Moved verbatim from the crate `CONTEXT.md`.
- **Contract A.8 execution-trace seam.** `TraceEvent` mirrors the oracle's `{trace,Pid,call,{M,F,Args}}` / `return_from` / `return_to` shapes; the engine **reports only** -- the host owns matching and term building. Emission (`src/interpreter/trace.rs`, gated on `Machine::trace_on`): `Call` for a local callee on `call*` and for an import on `call_ext*` (**entry only**); at a live `Continuation::Address` return, `ReturnFrom` then `ReturnTo`. Emission is infallible, so a traced run is byte-identical to an untraced one; **a natively-executed JIT block does not trace**.

## The interpreter has no arithmetic fast path (from `../../CONTEXT.md`)
Moved verbatim from the crate `CONTEXT.md` (which keeps the contract and a pointer).
- **Small-int `'+'`/`'-'`/`'*'` and `band`/`bor`/`bsr` are inlined only by the native baseline tier** (`src/jit/baseline/emit/arith.rs::try_inline_gc_bif`, consulted at `emit/block.rs:423-455`); the **interpreter has no arithmetic fast path** — every `gc_bif` goes out through `exec_bif` -> `ExecutionContext::call_bif` (`src/interpreter/dispatch.rs:341-349`, `src/interpreter/bif.rs:74-135`). The only interpreter-side saving there is the stack-array argument marshalling (`bif.rs:39-53`, no `Vec`).
