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
`mod.rs:105/113/146/349/384`) behind a new closure shape, and must keep `with_roots`' raise-latch parking
(`mod.rs:659-688`). The engine cannot derive the predicate itself: it does not depend on `beam_gc`
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

**Engine-held live terms and the published root set.** The two heap-term raise latches
(`raise_function_clause_args`/`pending_function_clause_args`) are **parked into the published root set**
by `Machine::with_roots` for the duration of every hand-off and re-seated from their slots afterwards (a
`with_roots` fast path hands over an unmodified `RootSet` when neither is armed), so a collecting host
relocates them with no host-side change; the supplied-trace latches are `Vec<StackFrame>` metadata and
need no rooting. `Machine::pending_exception` (`machine/catch.rs`) is the remaining engine-held live
reason `Eterm`: it is rooted **host-side**, in the park window, by `beam_rt`'s `Registers::extra_roots`
publication (read back through the write accessor `Machine::pending_exception_mut`), not by `with_roots`.
The complete set of `Eterm`-typed `Machine` fields is `x`/`fr`/`stack` (the `RootSet`), `pending_exception`, `class_atoms: [Eterm; 3]`
(atom **immediates** only — never a heap pointer, so no rooting needed), the two parked raise latches and
`args_scratch` (empty outside a dispatch). Everything else on the machine is `u32`/`bool`/`usize`
metadata or an `Arc<Module>` handle, so `current_code`, `raise_snapshot`, `entry_call`, the frame and
catch-frame stacks and the supplied-trace latches carry no heap term. The other Rust-local vectors are
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

## See Also
- `../CONTEXT.md` → the crate-level FROZEN API surface, execution model, constraints and handoffs.
- `../INSTRUCTION_SURFACE.md` → per-opcode/per-family semantics and the full known-gaps list.
- `../TEST_NOTES.md`, `../MEASURED.md` → the test/fixture enumeration and the measured figures.
