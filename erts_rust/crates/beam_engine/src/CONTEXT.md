# `beam_engine/src` — Engine Source Tree

## Intent
The implementation body of `beam_engine`: the typed-IR execution engine (direct-threaded interpreter,
module-context-aware driver, the nine instruction families, the term builders, the JIT tier seam).
This node owns `src/**` only; the crate's public API contract, the frozen seams and the exhaustive
execution semantics are documented one level up in `../CONTEXT.md` (read it first) and in
`../INSTRUCTION_SURFACE.md`.

## Cross-crate facts a `src/**` change must respect
- **`beam_engine::terms` is a PUBLIC module consumed cross-crate** by `beam_rt` and by
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
  (`interpreter/bif.rs`), which takes `ctx.call_bif_in_machine_outcome(self, entry_module, …)` when the gate is set
  and `ctx.call_bif_outcome(…)` otherwise (both `Result<BifOutcome>`, so the BIF suspend channel is available on
  every tier); a host that opts in must be reached on **every** tier, so keep the two
  drivers latching the same answer (`tests/bif_in_machine.rs` is the tier-parity gate). The run's **entry
  module** (context `0`, the module `Machine::current_stacktrace`'s `module` parameter expects for a
  context-0 frame) is threaded from the driver through `step_instruction_with_entry` → `exec_bif` /
  `call_bif_named` / `dispatch_wide` / `families::dispatch` → `dispatch_bif_call`, and on the native tier
  through `JitRuntime.entry_module` (a field generated code never reads).
- **The local-call seam's callee identity is memoized; the host's verdict is not.** `Machine::local_call_seam`
  (`interpreter/dispatch.rs`) names the same-module callee of every local `call`/`call_only`/`call_last` and asks
  `ExecutionContext::resolve_local_call(module, function, arity)` once per call. The three-value identity is a pure
  function of the immutable module image, so `Machine::local_callees` (`machine/local_callee.rs`) memoizes it per
  `(module address, function index)`; `Machine::reset` clears it, which is what keeps an address key ABA-free.
  The host is still consulted **exactly once per call** — a host spends one reduction unit per consultation, and
  `erlang:load_nif/2` can install a stub that a later call must be served from — so no verdict may ever be cached;
  an un-nameable callee is consulted zero times (a memoized sentinel), exactly as before the memo existed.
  A hit never reorders the memo and the keys live in a flat `[CalleeKey; 8]` beside the payloads, so the scan is
  inlined and stays inside two cache lines (a move-to-front policy was measured as an instruction regression: an
  alternating two-callee loop memmoved on every call). Both tiers drive this one definition, so the memo needs no
  `src/jit/**` change, and the interpreter's three local-call arms plus the baseline tier's probe stay identical.
- **`ExecutionContext::atoms` is the live atom namespace, for comparison AND for every raise.** The
  defaulted `fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable` (default body:
  `&module.atom_table`) is the namespace every **comparison** resolves names through: guards/
  `select_val`/map-key equality order atoms by NAME, and `beam_term`'s `erts_cmp_atoms` falls back to
  the raw atom id for a name the supplied table does not know — which is why a host owning one live
  namespace overrides the default. The same seam is the namespace every **raise** resolves its
  reason/class atoms through, via the free function `machine::catch::namespace_atom(ctx, module, name)`
  (`ctx.atoms(module).lookup(name)`, then the module snapshot, then `None`). Consequence for a host:
  one shared live table answers both, so `beam_rt` overrides `atoms` once (in `RuntimeContext` and
  `BudgetContext`) and gets name-exact comparisons *and* nameable
  `badarg`/`function_clause`/`error`/`exit`/`throw` on the raise path. A name **neither** source knows
  is a typed error at every one of those sites, never a substituted atom index (an index is valid only
  against the table it was read with). The names reachable through it: `function_clause` + the `error`
  class in the failed-clause raise, the reason + class of every BIF-raise landing (`interpreter/bif.rs`),
  the families' own tags (`badarg`/`badfun`/`badarity`/`badarith`/`badmatch`/`{badmap,…}`/`{badkey,…}`/
  `badrecord`), and the three class atoms installed once per run by `Machine::ensure_class_atoms`
  (called from `families::dispatch`).
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
  `resolve_external_call`/`resolve_local_fun`/`resolve_local_call`/`atom_name`; the record-table seam
  `foreign_record_definition` — `put_record`'s create form for another module's `{Module,Name}` id, where
  `Ok(Some(def))` = the host resolved it (`is_exported` is part of the answer) and `Ok(None)` = the
  family's typed `unsupported`; and `atoms`, the live atom
  namespace used by comparison AND by the raise path's `machine::catch::namespace_atom`), `RootSet`, `MinimalContext` (incl. its
  `ContextHeap` — a pointer-stable region heap over `beam_alloc::ReservedHeap`: ~4 MiB is reserved up
  front and **grown in place** as terms are built, so an allocation costs its payload only; on
  reservation exhaustion a fresh, larger region starts and **every earlier region is kept alive**, so a
  handed-out pointer stays valid and **unmoved** — `relocate` is deliberately never used. It does **not**
  override `alloc_words_rooted`, so it stays the test/embedding heap, not a collector; its
  `with_foreign_record_definitions`/`set_foreign_record_definitions` are the record-table hook behind
  `foreign_record_definition`, empty by default.)
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
- `terms/` → the boxed-term builders/readers (classic + `*_rooted`) in `construct`/`read`/`update`,
  plus `map`, `fun`, `number`, `slack`, `sink` and the `#[cfg(test)]` `test_support`/`alloc_tests` —
  `terms/CONTEXT.md`
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
The recorders are the two `pub(crate)` funnel hooks in `interpreter/dispatch.rs` — `on_instruction` before the instruction's effect, `on_error` for an `Err` out of `step_instruction_body` — gated on the latched `Machine::census_on` field each driver copies out of `is_enabled` once per run/resume entry, so a disabled census costs one load and a branch and **zero thread-local accesses per instruction**; a counted run is byte-identical to an uncounted one (asserted in `exec_census.rs`'s tests).
Observability limits, honestly: an `Err` escaping the funnel is an error, so a raise folded into a `Step` by a live in-machine `try`/`catch` frame counts as **executed but not error**; and a natively-inlined baseline-JIT instruction never reaches the funnel, so `Tier::Interpreter` is the fully covered tier while a native tier reports only its deopted steps.
`write_dump(section) -> Option<PathBuf>` writes `std::env::temp_dir()/beam_engine_exec_census_<section>.tsv` (a `#` totals line, the `variant`/`executed`/`errors`/`first_error` header, one row per variant, `-` when a variant never errored; any `section` character that is not alphanumeric/`-`/`_` becomes `_`), the machine-readable form of a snapshot; a write failure is reported on stderr and yields `None`.
The instrument's own ranked view is `jit/baseline/census.rs`'s runtime **execution** census over the `jit::bench` workload set at `Tier::Interpreter`, and the corpus-scale ranking is `tests/dynamic_census.rs`: both are measured figures and live in `../MEASURED.md`, never here — and that workload set is the in-crate tier set, not the OTP corpus.

## GC publication: which `src/**` engine→host calls can be intervened on
The engine hands a host its root set through **exactly three** doors, all under `src/machine/`: `Machine::with_roots` (the `RootSet` carrier and the only hand-off body, `machine/mod.rs`), `Machine::alloc_rooted` (→ `ExecutionContext::alloc_words_rooted`, `machine/mod.rs`) and `Machine::ensure_heap` (→ `ExecutionContext::ensure_heap_full`, `machine/frames.rs`).
`with_roots` hands over exactly what the run has **used**: `&mut self.x[..used_x_extent]`, the whole
`&mut self.stack` (every Y slot) and `&mut self.fr[..used_fr_extent]`, where the used extent is
`max(x_mark, x_dirty)` / `max(fr_mark, fr_dirty)` clamped to the file — `machine::registers::used_extent`,
the **single rule** the hand-off and `Machine::reset`'s refill share. `x_dirty`/`fr_dirty` name the extent
from `x_mut`/`fr_mut` (whole file), from `with_roots` (the root prefix it hands over) and from the native
driver (`CompiledModule::native_write_extent`, noted **before** the block runs). Every slot at or beyond the
extent is `NIL` by the refill invariant, so it is not a root; a host therefore relocates and writes a moved
term back **only inside the slices it receives** (it physically cannot write outside them), and that
hand-off — not a host promise — is what bounds the next reset to the words the run actually used.
`live` is still **only a hint** (`RootSet::live_len()` clamps it to the handed slice, `src/context.rs`): a
host that scans just `x[..live_len()]` leaves any word above `live` unrelocated, so the hint's provenance
is a contract: the two **heap** doors pass the instruction's own `Live` operand verbatim
(`interpreter/dispatch.rs`'s `Instr::TestHeap`/`Instr::AllocateHeap` arms ← `beam_loader`'s operand), while
**every** `alloc_rooted` caller passes `RootSet::ALL_LIVE` — the hint is unchanged by the narrower
hand-off. The engine computes no liveness of its own.
Their call sites, exactly: `alloc_words_rooted` has **one** production caller — `terms::reserve`
(`src/terms/slack.rs`, hence `reserve_slack` and every `*_rooted` builder; `terms::test_support` is
`#[cfg(test)]`), reached **per built term** (each `put_list`/`put_tuple2`/`make_fun3`/
`put_map_assoc`/record/bitstring node), with `RootSet::ALL_LIVE` at every family site.
The classic unrooted builders (`make_tuple`/`make_cons`/… in `terms/construct.rs`, `terms/number.rs`,
`terms/map.rs`, `terms/fun.rs`, `terms/sink.rs`) are the host/test API and reach
`ExecutionContext::alloc_words` directly, with neither a root set nor an admission point; **every in-crate
construction arm** uses a `*_rooted` twin instead — that rule, not a census of `alloc_words` callers, is what a
new engine-side build must follow.
`ensure_heap_full` is reached **only** from the interpreter's `Instr::TestHeap`/`Instr::AllocateHeap`
arms (`interpreter/dispatch.rs`) plus their baseline-JIT twins `jit_allocate_heap`/`jit_test_heap`
(`jit/baseline/runtime.rs`), so it is a **per-`test_heap`** door, never a per-allocation one;
`Instr::Allocate` pushes the frame with **no** admission.
`ExecutionContext::heap_available` has **no** call site anywhere in the crate (introspection only).
**The off-heap reservoir is on no door at all.** `Machine::refc_binaries` (`mod.rs:349`) is reachable only
as `&self` (`refc_binaries()` `:1114`, `off_heap_overhead_bytes()` `machine/offheap.rs:78`), so a host at
`call_bif_in_machine` (`context.rs:413`) can *price* the reservoir but never prune it; no seam hands out
`&mut Machine`. `retain_refc_binaries` (`machine/offheap.rs:126`) takes `&mut self`, so it also cannot run
while a `RootSet` built from `&mut self.x`/`stack`/`fr` is alive — a prune hosted at a door needs a
field-level split (the three register files and `refc_binaries`/`writable_payloads` are disjoint fields,
`mod.rs:105/113/146/349/384`) behind a new closure shape, and must keep `with_roots`' loose-heap-term parking
(`Machine::loose_heap_terms`, `machine/mod.rs`). The engine cannot derive the predicate itself: it does not depend on `beam_gc`
(`Cargo.toml:8-13`). An in-slice prune is also unsound for the accumulator **in flight** — the roots do not
name a `bs_init_writable` payload until its term is written back, while `retain_refc_binaries` prunes the
writable registry in lockstep (`offheap.rs:138-142`).
Every other engine→host call carries **no** root set, so a host that
*allocated* inside it would relocate the young heap against unpublished roots. Those calls, with the
state each holds in a Rust local across the call:
- `ExecutionContext::resolve_local_fun` (`families::funs`' `call_fun`) — the `fun` Eterm (read by
  `FunSource::read` from `x(arity)` or the `call_fun2` operand) and the `args` copy
  (`families::funs::register_args`). `terms::fun_free` (`terms/fun.rs`) then re-reads the term's **raw
  heap storage** *after* the call, so a move in the seam would dangle both.
- `ExecutionContext::apply_fun_outcome` (the `Ok(None)` fallback of the same `call_fun`, whose default
  body calls `apply_fun`) — the same `fun`/`args`; protection here is the host's relocation exemption,
  not publication. A `FunOutcome::Suspend` writes nothing, so the parked instruction's operands stay
  readable from `x(arity)`/`x(0..arity)` for the resume's re-consultation.
- `ExecutionContext::call_bif` (`interpreter/bif.rs`) — the marshalled arguments are **copies** living in
  the `EMPTY_ARGS` stack array (`exec_bif`/`call_bif_named`) or `Machine::args_scratch` (**taken out of
  the machine** for the whole `dispatch_wide` call and restored afterwards, so it is not even reachable
  from `Machine` while the terms are live); **neither is in any `RootSet`**. The in-machine twin
  `ExecutionContext::call_bif_in_machine` is reached from `Machine::dispatch_bif_call`, which takes
  **`&self`** — so at that door a host can neither see the arguments in a root set NOR write a relocated
  word back into `x`: a collection during it leaves both the copy and the register file dangling. The
  trait documents no GC contract for `args`.

**Verified: no admission point can intervene on the `call_fun`/`call_fun2` path with the shipped host.**
`BudgetContext::resolve_local_fun` (`beam_rt/src/runner/budget.rs`) forwards **without** arming, and
`BudgetContext` arms only around the doors it prices, each immediately followed by the delegate whose
first statement takes the latch — so the latch can never be armed-and-unconsumed across that seam call.
`RuntimeContext::resolve_local_fun` → `local_fun_target` (`beam_rt/src/context/apply.rs`) is `&self` and
does no BEAM-heap allocation (only Rust-heap `Vec`s). The same holds for the BIF-in-flight window: the
`gc_bif` admission point runs *before* marshalling and `BudgetContext::call_bif` does not arm.

Consequences for a `src/**` change: the `Ok(Some)` copy loop needs **no** rooted builder — there is no
allocation to reserve, so `with_roots`/`alloc_rooted` do not apply (and cannot: the seam takes no
`RootSet`). If a host ever made the seam allocating, the sound fix is to **re-read** `fun` from
`x(arity)`/the operand and re-collect `args` after the seam call and before `fun_free`, never to
"publish" (there is no door to publish through).

**Engine-held live terms and the published root set.** The machine's loose heap terms
(`Machine::loose_heap_terms`) are **parked into the published root set** by `Machine::with_roots` for the
duration of every hand-off and re-seated from their slots afterwards (a `with_roots` fast path hands over an
unmodified `RootSet` when none is armed), so a collecting host relocates them with no host-side change.
The parked set is the two failed-clause argument latches
(`raise_function_clause_args`/`pending_function_clause_args`) **and the pending exception's reason**
(`Machine::pending_exception`, `machine/catch.rs`) — the field the engine does not clear at landing, so it stays
readable for the rest of the run and is a live root at every door by construction.
`beam_rt`'s park publication of that reason through `Registers::extra_roots` (read back through
`Machine::pending_exception_mut`) is therefore reading an already-relocated word, not supplying the relocation.
The supplied-trace latches are metadata — `pending_supplied_trace`/`raise_supplied_trace` (`Option<StackTrace>`) —
and need no rooting, and so are the two **term** latches `pending_supplied_term`/`raise_supplied_term`
(`Option<Eterm>`): nothing allocates between `Instr::RawRaise` arming one (`Machine::raise_supplied`) and
`Machine::throw_with_trace` promoting it, and the promotion's destination is either the handler's `x(2)` (inside
the `RootSet`) or the uncaught funnel's rendering, which happens before any admission.
The complete set of `Eterm`-typed `Machine` fields is `x`/`fr`/`stack` (the `RootSet`), the three parked loose
terms above, `class_atoms: [Eterm; 3]`
(atom **immediates** only — never a heap pointer, so no rooting needed) and `args_scratch` (empty outside a dispatch). Everything else on the machine is `u32`/`bool`/`usize`
metadata or an `Arc<Module>` handle, so `current_code`, `raise_snapshot`, `entry_call`, the frame and
catch-frame stacks and the symbolic supplied-trace latches carry no heap term. The other Rust-local vectors are
recorded in `../../beam_rt/M1_GAPS.md` and `../../beam_rt/src/context/CONTEXT.md`.

## Execution position (`Machine::current_code`) — the complete store set
The position has **two** writers plus the `reset` clear: `Machine::step_instruction_with_entry`
(`interpreter/dispatch.rs`) writes the field directly at every interpreted instruction's entry — the
instruction-granular store every suspension and raise relies on — and `Machine::set_current_code`
(`machine/location.rs`, a plain `self.current_code = Some(..)`) is the accessor `src/jit/**` reaches;
`Machine::reset` clears it to `None`.
Every store is an instruction boundary, but **not** every store happens before the thing it names runs:
- Interpreter: `Machine::drive` stores **before** the charge and the body, so a `reduce` refusal parks on
  the **not-yet-executed** instruction; then `step_instruction_with_entry` re-stores the same address.
- Native baseline tier, driver head (`drive` in `jit/baseline/mod.rs`): once per driver-loop iteration,
  in the order `set_current_code(func, pc)` → the `Machine::reductions` bump → `ExecutionContext::reduce(1)`
  → `code.enter`, so a refusal parks at the block entry with the block unexecuted.
- Native block prologue (`jit/baseline/emit/prologue.rs::emit_block_prologue`): the **block's entry** pc,
  through `jit_set_position`, or — when `Allocation::prologue_publishes_y` holds — through
  `jit_publish_y`, which makes the same store **and** re-publishes the frame's Y window in one call; so
  mid-block the position is a block entry and a resume from it **re-executes the block prefix**.
- Exact per-instruction helpers, each storing the instruction it is about to run: `jit_step_ext`,
  `jit_family` (shared with `jit_receive`, which is a thin wrapper) and `jit_test`, plus the frame/heap
  helpers `jit_deallocate`, `jit_return`, `jit_deallocate_return`, `jit_allocate`, `jit_allocate_heap`,
  `jit_test_heap`, `jit_trim` — all in `jit/baseline/runtime.rs`. `resume_in_block` stores the address it
  is about to resume at, in the driver loop head's own order; a `jit_step_one` deopt reaches the store
  through `step_instruction_with_entry`.
- **Seam routes that store NOTHING**, so a host hook reached through them runs with a stale
  (block-entry-or-earlier) position: `jit_ext_call` (`resolve_ext_entry` asks
  `ctx.resolve_external_call` and hands the driver the `ENTER` word), `jit_local_call`/`probe_local_call`
  (the local-call seam: a *served* callee's store happens afterwards, in `call.rs::store_continuation`'s
  served arm, and a *declined* one transfers in-block through `enter_in_block`, which does store the
  callee entry), `jit_bif_value` (declines to `jit_step_one`), and `jit_tail_call` (a checked frame pop
  only — the successor's prologue or the driver stores next).
  This is why `run_baseline_cached_resume_outcome` must not assume an exact instruction position on a
  suspended cross-module/local-call frame.

## Long files (role-justified, beyond the ~1000-line guideline)
The ~1000-line guideline holds for this node's own top-level files except the one below — one cohesive unit (the whole runtime seam), kept whole because its size follows from that single responsibility, not from unrelated code accumulating.
Files inside a subtree with its own `CONTEXT.md` (`machine/`, `terms/`, `interpreter/`, `families/`, `jit/`, `bench/`) are documented by that node's `CONTEXT.md` and not listed here.
- `context.rs` (2 184 lines) — the entire runtime seam: `ExecutionContext` + `Mailbox` + `RootSet` + the
  `MinimalContext` region heap.
`exec_census.rs` (651) is the largest top-level file after it; every other `src/*.rs` is under 450.

## Per-module unit-test coverage
Every `src/**` module carries `#[cfg(test)]` unit tests beside the code they cover (the integration suites are the sibling node `../tests/`).
The exhaustive enumeration follows, one item per module (or per sub-suite where a module splits its tests):
`context` (hook dispatch, default-hook unsupported, the `alloc_words` arena pointer stability/aliasing, `apply`/`apply_fun` forwarding and their defaults, the apply hook); `machine` (the cross-cutting raise-funnel suite in `src/machine/tests.rs` — every raise kind propagated through a real run entry, a caught raise unchanged, and the **raise-time snapshot**: `throw_with_trace` captures the pre-unwind `(context, func, pc)` chain on **both** the caught and the uncaught path, an already-unwound caller is still named, `Machine::reset` clears it and a resume preserves it); `machine::registers` (the x/FR files, their bounds and `reset`); `machine::frames` (frame push/pop/return, `trim`, Y-slot scoping and bounds, `ensure_heap`'s root-set forwarding, `resumption_addresses`, `raise_snapshot`); `machine::catch` (the catch-frame stack, the throw/unwind/landing paths, `pending_exception`/`pending_class`, and the `with_uncaught_frames` funnel — it attaches only to an empty raise, leaves a diagnostic unframed, and resolves each frame in its own module context); `machine::run` (`run_export`/`run_label`, `RunOutcome`, the blocked-receive fold and `resume_outcome`); `machine::drive` (the per-instruction reduction charge and the slice counter); `machine::location` (the `wait_deadline`/`timeout_delay` accessors over a literal and a Y-register delay); `predicates` (the `beam_term` boxed-term predicate wrappers, incl. native-record vs tuple); `terms` (every constructor↔reader round-trip through `MinimalContext` — incl. `make_map` across the 32-key flat/hashmap boundary and the one-reservation carve — raw-word layout proofs, both map readers over hand-built nodes, and the error cases: `make_flatmap` >32 / fun field >255 / binary >64 → `unsupported`/`SystemLimit`, wrong-type readers → `BadArg`); `terms::alloc_tests` (the **rooted-allocation** suite: a counting context proves the `*_rooted` builders fire the rooted hook only, a semi-space **moving** collector proves the reserve → read → write ordering, and a peak-word table contrasts the never-freeing arena with the collecting one); `operand` (read/write each supported kind incl. `FloatReg`↔FR file and out-of-range, `Literal` via `Machine::literal` and a missing one → typed error; `Label`/`List`/`Allocation` → unsupported); `interpreter` (the label map + resolution, and — in `src/interpreter/tests.rs` — the `NO_FAIL` guard raises: a failed `test`/`select_val` with `fail == NO_FAIL` raises class `error` with the bare atom `function_clause`, once through `step_instruction` and once through `run_label`, landing in a live `try` frame, while a module that cannot name the atom is the typed `unsupported_opcode: <site>: no 'function_clause' atom in the module`); `jit` (the roster invariants — interpreter first, `"baseline-jit"` always present, `"jit-cranelift"` iff the feature is on, the roster is a `Tier::ALL` prefix; `Tier::name`↔`from_name` round-trips + unknown/empty/miscased names ⇒ `None`; `run_with_tier` on the reused `slice.beam` fixture — `Interpreter` equals `run_export`, `BaselineJit` equals it too, `Cranelift` is a typed error); `jit::baseline` (block-target `encode_target`/`decode_target` round-trips and `HALT`/`ERROR` sentinel distinctness, the `JitRuntime` field offsets generated code relies on, that cached slots index the x-register array rather than the `Machine` struct, the cached/directly-addressed register allocation and its bound, and the `constant_word`/`x_register` operand forms); `jit::baseline::emit::link` (the direct cross-block `jmp` linkage — an emitted `jmp rel32` landing on the successor's compiled entry, the `rt` reload on a linked exit, and the cached-slot spill); `jit::baseline::emit::test` (the `test_plan` inline/refuse decision: the word tests plus the boxed-term `TestKind`s it lowers, and the refusals — `NO_FAIL`, an unresolved label, an `arity == 0` tagged tuple, a missing `true`/`false` for `is_boolean`); `jit::cache` (the two-part-keyed compiled-block cache — `module_fingerprint` stability across reloads, same-name/different-content non-collision, the namespace partition `two_loads_of_one_beam_are_two_namespaces_and_two_entries`, `get_or_compile` reuse, and `clear`); `jit::emitter` (the encoder is proven by *executing* what it emits: `add`, a six-argument sum (`r8`/`r9`, `r12`/`rsp` addressing), `lea`, `cmp`+`jcc`, an indirect `call_r`, label patching, `seal` flipping `/proc/self/maps` to `r-xp`, and the error paths); `jit::tiering` (the cold/warm/hot boundaries, the `hot ≥ baseline` clamp, a zero baseline threshold, equal thresholds skipping the baseline rung, `Default`, `is_hot`, and that `select` never names a tier outside `available_tiers`); `jit::cranelift` (the feature-gated backend: tier name/availability, one native entry per non-empty function, the empty module, and a **direct call into a compiled entry** proving native execution); `families::*` (per-family unit tests, plus the `families::dispatch` tests in `families/mod.rs` — `families::{bitstrings/{create,matching,raw,run_tests},exceptions,floats,funs,maps,receive,records}`, and the split families group their suites further: `families::exceptions::tests::{handlers,raising,stacktrace,end_to_end}` (the `stacktrace` sub-suite cooks the non-`NIL` arm from the **raise-time snapshot**, incl. an already-unwound caller and a frame resolving in its own registered module context) and `families::records::tests::{native,elements,update,access,field,foreign_create}`; `families::funs::tests` also owns the four `apply`/`apply_last` cross-module-seam tests — the `apply` `Step::Enter` + caller return address, the cross-module receive suspend/resume, the `Ok(None)` sync-path fallback, and `apply_last` popping its frame first; `records::tests::update` keeps the update paths plus the create form's no-host typed errors, while `records::tests::foreign_create` (14 tests) is the create form against a **host record table** — the `foreign_record_definition` seam's exported, non-exported and `Ok(None)` branches (the `badrecord` one on both the `NO_FAIL` raise and a live `fail` label), the `{SelfModule,Name}` export gate vs the atom-`Id` local form, `badfield` outranking `no_value`, the first-absent-field `no_value`, `Some(small 0)` vs an absent default, the unnameable-name rows and interpreter/baseline-tier parity); `families::bitstrings::run_tests` is the family's end-to-end + oracle suite (always through `Machine::run_label`/`run_export`, never a family helper): a non-byte-aligned refc subject read bit-for-bit through `bs_start_match3`/`bs_get_tail`/`bs_get_integer2`, an `append` segment in a **non-first** position, and a live `erlc`/`erl` cross-check of an inline module ; `families::maps::tests` pins the read instructions' `NO_FAIL` raise (a missing key with `fail == NO_FAIL` raises the bare atom `function_clause`, caught and uncaught, while a module that cannot name the atom is the typed `unsupported_opcode: maps: no 'function_clause' atom in the module`), beside the unchanged `{badmap,_}`/`{badkey,_}` write-path raises); `exec_census` (the measurement instrument's **8** unit tests: off by default and `reset` zeroing the counts, a `Test` instruction keyed by its kind, `first_error` retaining only the first text, the TSV dump's shape and its section-name sanitisation, the executed count taken through the interpreter funnel, a raising instruction counted as **executed *and* errored**, and per-thread isolation).
## Root-set contract: the two admission doors and the reserve -> read -> write discipline (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **Root-set contract made concrete, with both admission doors defaulted.** The primary admission point is the defaulted `ensure_heap_full(heap_need, roots)` over `RootSet`, which `Machine::ensure_heap` calls so the context gets the exact GC root set (`x[..used_x_extent()]` + the whole stack + `fr[..used_fr_extent()]`) and a Stage-4 collector can move terms in place without frame liveness maps; its default forwards to the frozen narrow `ensure_heap(heap_need, live, x_regs, stack)` with the FR file discarded, so an existing host is byte-identical. The **second** admission point is `alloc_words_rooted(n, roots)` over `RootSet` (`x`/`stack`/`fr`, all mutable, plus the `live` hint) -- the root hand-off for *term construction*, because `alloc_words` itself has neither an admission point nor roots; its default discards the roots and forwards to `alloc_words`, so an existing host is byte-identical. **Every in-crate term-construction arm routes through the second door** via the `terms::*_rooted` builders, which follow the frozen **reserve → read → write** discipline (`reserve` the block first with *exactly one* rooted allocation, then read operands from the machine's post-collection state, then write the header/element words); `src/terms/slack.rs` is the reserve seam and `src/terms/sink.rs` the `Sink` shape writer a multi-node build is carved from. `terms` is the crate's dedicated raw-heap accessor module (the per-family `raw` modules and the JIT encoder are the documented layout exceptions, listed in `CONTEXT.md`); every in-crate construction **arm** goes through a `*_rooted` twin, while the classic unrooted builders (`terms/{construct,number,map,fun,sink}.rs`, plus the four `&self` execution-position accessors in `src/machine/location.rs` that cannot lend `&mut` roots) are the host/test API and are reachable in production only from a caller that already cannot collect.

## Who owns the collecting host (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **The collecting host is `beam_rt`'s side, not an engine gap.** `MinimalContext` has an unbounded, never-freed reserved-heap-region arena, and `test_heap`/`allocate_heap` reach the full-root admission point (`ensure_heap_full`, a no-op there -- its default forwards to the no-op `ensure_heap`). The engine's own half is landed: **every** in-crate term-construction arm allocates through the rooted door (`alloc_words_rooted` / `Machine::alloc_rooted` / the `terms::*_rooted` builders), under the frozen reserve → read → write ordering, proved under a mock **moving** collector by `src/terms/alloc_tests.rs` (the rooted-vs-unrooted peak/retained live-words table) and `tests/rooted_alloc.rs`, with the root hand-off pinned by `tests/gc_roots.rs`. **`beam_rt`, the production heap, overrides both rooted doors** (`beam_rt/src/context/execution.rs`) -- it publishes the whole `RootSet` it is handed (the used X/FR prefixes plus the whole stack), runs `beam_gc` over it, writes the moved terms back, and declines a collection whose machine root reaches a container in a region the collector does not scan -- so an ordinary `beam_rt` run collects at the engine's heap-admission point, conservatively gated (`beam_rt/src/context/gc.rs`: a process is pinned once a live float register is seen, halted on a collection error, and a global valve disables collection once an escaped runtime store holds an off-heap term). A collecting host must additionally root what the `RootSet` cannot carry: `Machine.pending_exception` (`src/machine/catch.rs`, reachable mutably through `Machine::pending_exception_mut`) and the `Eterm` arguments parked in `Machine.args_scratch`/the `exec_bif` stack array while a `call_bif` is in flight. Detail: `src/CONTEXT.md` (`## GC publication…`).

## The comparison atom-namespace seam (`ExecutionContext::atoms`) (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **Atom comparison is name-exact end-to-end, through a defaulted live-namespace seam.** Every guard/`select_val`/map-key comparison site resolves atom *names* through the defaulted seam `ExecutionContext::atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable` (`Machine::eval_test`'s six comparison kinds + `is_boolean` in `src/interpreter/test.rs`, `Instr::SelectVal` in `src/interpreter/dispatch.rs`, the native baseline tier's `evaluate_comparison` in `src/jit/baseline/test_helper.rs`, and the map key ordering/lookup in `src/families/maps/{read,write}.rs`), so atom order matches the `<` BIF, which compares through `ctx.atoms()` — the runtime's ONE shared live table (`beam_bifs/src/compare.rs:104-121`). The seam's **default** is `&module.atom_table`, the **load-time snapshot** (`beam_rt/src/codeserver.rs:573` — `lower::build_module(&chunks, &raw, (*self.atoms).clone())`), which keeps every host that owns no shared namespace working; `beam_rt` **overrides** it with the live table in BOTH of its `ExecutionContext` impls — `RuntimeContext` (`beam_rt/src/context/execution.rs`) and the scheduled path's `BudgetContext` (`beam_rt/src/runner/budget.rs`, which must forward the seam explicitly because it forwards every method by hand). Without that override, an atom interned *after* a module was loaded is absent from the comparison table, and `beam_term::cmp_atoms` (`beam_term/src/compare.rs:356-363`) falls back to raw atom **index**, which has nothing to do with alphabetical order — a wrong guard branch with **no error surfaced**. Pinned by the oracle-cross-checked `src/interpreter/test.rs::tests::comparisons_order_atoms_by_name_through_the_shared_namespace` and `beam_rt/tests/atom_namespace.rs` (the real `sofs:drestriction/2` repro plus a guard over an atom interned after load via `binary_to_term`, on both the `RuntimeContext` and `BudgetContext` routes).

## The BIF suspend channel: the full host contract (from `../CONTEXT.md`)
Moved verbatim from the crate `CONTEXT.md` (which keeps the contract and a pointer).
`ExecutionContext` carries two **outcome-aware** BIF doors, one beside each of the value-returning ones: `call_bif_outcome` (`call_bif`'s twin) and `call_bif_in_machine_outcome` (`call_bif_in_machine`'s twin).
Both return `Result<BifOutcome>` and both **default** to forwarding the value wrapped as `BifOutcome::Return`, so a host that predates the channel is byte-identical; `Machine::dispatch_bif_call` takes exactly one of them per run.
A host that answered `true` from `wants_bif_in_machine` is dispatched through the in-machine door only, so it must override `call_bif_in_machine_outcome` to make its BIFs able to wait -- the engine never reaches its `call_bif_outcome`.
`BifOutcome::Return(value)` is the ordinary answer; `BifOutcome::Suspend` means the BIF did **not** complete and the instruction was **not** applied (no destination register written, no guard-failure branch taken, no frame effect).
The park/resume protocol: the engine reports `RunOutcome::Suspend { func, pc }` **at the BIF instruction itself**, the host pumps its own waiting resource, and `Machine::resume_outcome`/`resume_outcome_with_tier` re-enters there -- re-executing the instruction, which re-issues the BIF with the **same operands**, so the caller sees *that* call's value and never a value the abandoned attempt computed.
The host side must be re-execution safe on its own too: a suspending attempt must not consume a one-shot resource.
The rooting argument: the engine holds **no** suspended-BIF state -- no `Machine` field, no saved argument vector, not even the BIF's identity -- because the parked state is `(func, pc)` plus the registers, the Erlang stack and the FR file, i.e. exactly the frozen `RootSet` (`x[..live]`, the whole stack, the whole FR file) a host is handed at every admission point, so a moving collector may relocate the operands and the re-execution re-reads the relocated words.
(The C equivalent is the process-private `c_arg_reg`/`arg_reg` array that `BIF_TRAP_N` writes, saved by `copy_out_registers` and restored by `copy_in_registers` -- `../../erts/emulator/beam/beam_common.c:306`/`:336`.)
**Class A vs class B**: a re-execution-idempotent site (pure operand reads, every write success-only) turns a suspend into the park, while the `erlang:apply/3` route -- which has already walked the argument list into `x(0..arity)` -- reports the typed `unsupported("bif suspend at a non-idempotent dispatch site: apply/3 rewrites x(0..arity)")` instead, because re-executing it would re-read clobbered registers and apply a different call.
Both tiers report and resume a suspend: the interpreter's driver folds `Step::Suspend` into `RunOutcome::Suspend` (`src/machine/drive.rs`) and the native baseline tier carries it out of a compiled block as the `SUSPEND` (`-5`) block-ABI word with its address in the `JitRuntime::suspend` side channel; `Tier::Cranelift` cannot resume at a mid-function `pc` and keeps its existing honest typed `unsupported`.
A **non-blocking** entry (`run_export`/`run_label` and the tiers' non-blocking run) folds a suspend into the typed `error::suspended()` -- never `error::blocked()`, never a substituted value.
Tracing deviation: a `call_ext*` arm emits its `call` event **before** the dispatch, so a suspend on a traced run re-emits that event when the instruction is re-executed.
Gate: `tests/bif_suspend.rs`.

## The closure-apply suspend channel: the full host contract (from `../CONTEXT.md`)
Moved verbatim from the crate `CONTEXT.md` (which keeps the contract and a pointer).
`apply_fun_outcome` is the outcome-aware twin beside the value-returning `apply_fun`, returning `Result<FunOutcome>` (`Return(value)` / `Suspend`) and **defaulting** to forwarding that value wrapped as `FunOutcome::Return`, so a host that predates it is byte-identical (an unimplemented `apply_fun` still answers `unsupported("apply_fun")`).
It exists for the same reason as the twin BIF channel, one level down: a host that **interprets** the closure itself (a tree-walking `-eval` evaluator) has no way to park its caller when the body must wait, and `apply_fun` can only answer a value or an error.
`FunOutcome::Suspend` means the application did **not** complete and the instruction was **not** applied; the engine parks on the `call_fun`/`call_fun2` instruction (`Step::Suspend`, `RunOutcome::Suspend`) and the resume **re-consults this door with the same `fun` and `args`**, so the host must **continue** the pending application and never restart it (an evaluator body is not re-execution idempotent) -- the one respect in which its contract is stricter than `BifOutcome`, whose re-issued dispatch re-runs the BIF from its operands.
Gate: `tests/fun_suspend.rs`.

## See Also
- `../CONTEXT.md` → the crate-level FROZEN API surface, execution model, constraints and handoffs.
- `../INSTRUCTION_SURFACE.md` → per-opcode/per-family semantics and the full known-gaps list.
- `../TEST_NOTES.md`, `../MEASURED.md` → the test/fixture enumeration and the measured figures.
