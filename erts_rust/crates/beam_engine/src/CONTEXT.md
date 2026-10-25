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
- **The local-call seam memoizes the host's verdict, per callee function index, per run.** `Machine::local_call_seam`
  (`interpreter/dispatch.rs`) names the same-module callee of every local `call`/`call_only`/`call_last` and can ask
  `ExecutionContext::resolve_local_call(module, function, arity)`. The identity and the verdict are pure functions of
  the immutable module image plus the host's answer set, so `LocalVerdicts` (`machine/local_callee.rs`) holds both in
  the executing module context's table — one `Cell<Verdict>` slot per entry of `Module::functions` — and a repeat local
  call reads the callee's own slot; `resolve_local_call` is therefore consulted **once per distinct callee per run**,
  and an un-nameable callee zero times (a memoized sentinel). Caching the verdict is the *faithful* choice: ERTS charges
  nothing for an intra-module call, so a per-call consultation spends a reduction unit the oracle never charges.
  Three doors invalidate — `Machine::reset` (which drops the contexts that own the tables, and clears the entry table
  with them), the BIF-dispatch funnel `Machine::dispatch_bif_call` when it dispatches `erlang:load_nif/2` (the BIF
  installs the stub a later local call must be served from; the funnel is `&self`, so it only **arms** a `Cell` flag
  and the seam clears the verdicts at its next local call), and the public
  `Machine::invalidate_local_call_verdicts` for a host whose answer set changes by any other means.
  The hot path is the slot read alone: no scan, no key compare, no data-dependent branch. Both tiers drive this one
  definition, so the memo needs no `src/jit/**` change. What is **not** legitimate is baking a verdict into
  **compiled code**: `JitBackend::compile(&mut self, module: &Module)` is handed no `ExecutionContext`, and the
  cross-run `CodeCache` is content-keyed and shared across runs, machines and hosts — the memo is a per-machine,
  per-run, invalidatable cache and nothing the emitter generates may carry one.
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
  `beam_alloc::ReservedHeap` bump seam — its sibling `context/reclaim.rs`, the same seam's rewind
  bump-pointer advance and the `beam_gc::FromSet`/`LinearDest` construction over those reservations —
  `families/bitstrings/raw.rs`, `families/records/raw.rs`,
  `jit/emitter/*`, `jit/baseline/*`, feature-gated `jit/cranelift/*`). Errors are
  `beam_common::{Result, BeamError}` only, and unsupported cases stay typed errors.
- **A term produced in a `MinimalContext` arena dies with that context.** The arena owns
  the only copy of the words, a `Machine` owns no heap, and dropping the context returns
  the carrier blocks to `beam_alloc`'s PROCESS-GLOBAL segment pool, where the next arena
  (another tier's, or a parallel test's) writes over them. Reading such a term afterwards
  is a use-after-free that surfaces as a **wrong value** or the term-integrity abort
  `beam_term::ops::class: invalid boxed header subtag <n>` — never a typed error, and only
  intermittently (it needs the recycled slot to hold a non-header word, so it can hide for
  thousands of runs and then fire once under load). A test that compares terms from two
  runs must keep **both** hosts alive to the comparison, or compare inside each run's own
  scope. Contract text: the `MinimalContext` docs in `context.rs`.

## Routing Table
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `context.rs` → the `ExecutionContext` / `Mailbox` seams (`call_bif` + the defaulted in-machine twin
  `call_bif_in_machine` behind the `wants_bif_in_machine` probe; the resolver seams
  `resolve_external_call`/`resolve_local_fun`/`resolve_local_call`/`atom_name`; the record-table seam
  `foreign_record_definition` — `put_record`'s create form for another module's `{Module,Name}` id, where
  `Ok(Some(def))` = the host resolved it (`is_exported` is part of the answer) and `Ok(None)` = the
  family's typed `unsupported`; and `atoms`, the live atom
  namespace used by comparison AND by the raise path's `machine::catch::namespace_atom`), `RootSet`, `MinimalContext` (incl. its
  `ContextHeap` — a pointer-stable region heap (`regions: Vec<ReservedHeap>`, oldest first, the last one
  is the allocation frontier) whose growth arithmetic is: `try_grow(n)` first (committed limit
  raised **in place** to exactly `used + n`, no heuristic — `reserved_heap.rs:194-205`), else a fresh
  region with `reserved = prev.reserved*2` floored at `HEAP_RESERVE_WORDS` (**4 MiB**) and raised to at
  least `n`, and `committed = HEAP_COMMIT_WORDS` (**64 KiB**) raised to `n` and clamped to `reserved`
  (`context.rs:1550-1575`). Regions are reserved from `beam_alloc` carriers (anonymous `mmap`), so a
  reservation is address space only. The grid **grows by `push` and is trimmed only by the two
  host-driven reclaim tiers** (`regions.truncate(..)` in the Tier-A rewind, `regions.clear()` in the
  Tier-B collapse and in `abandon`; `relocate` is deliberately never used), and no path the crate itself
  takes ever frees, truncates, resets or reuses a region — an arena whose host calls no tier behaves
  exactly as one that could never reclaim, and a region's carrier `mmap` is released by its `Drop` alone
  (`ReservedHeap::reset` frees no RSS). Its **measurement + mark surface**: `heap_stats() ->
  ContextHeapStats` (`regions`, `reserved_words`, `committed_words`, `used_words` — **cumulative
  demand**, never live memory, `= live_words + released_words` — the `live_words` Σ `used` figure the
  regions hold *right now* and the one a reclaim lowers, `released_words` the words the tiers have taken
  back and never given up, `peak_words` and the frontier's `frontier_used`/`frontier_available`),
  `heap_mark() -> HeapMark` (`EMPTY` until the first region; it records the frontier's region index, base
  address and used words, plus the private **epoch** it was taken in), the typed predicate
  `heap_mark_matches(HeapMark) -> bool` (a **stale** — replaced-region or another arena's —, **forward**
  or **pre-reclaim** mark is `false`), and the census walk `for_each_heap_region(impl FnMut(&mut
  [Eterm]))` — every region's written prefix `[0, used)`, oldest first, whose visitor must **not**
  interpret the words as terms (they are raw payload) and must not keep the slice. **A mark is a
  precondition check, never a permission**: it carries no liveness claim, and both tiers act on the
  caller's own assertion (nothing reachable at or above the mark / outside the forwarded roots); each
  bumps the epoch on success, so every mark taken before a rewind or a reclaim is refused afterwards by
  both tiers and by `heap_mark_matches`. **Neither tier is ever invoked by the crate itself** — no
  allocation, admission or `heap_available` path reaches them — and their contracts, report types and
  refusals belong to the child node: `context/CONTEXT.md`.
  It does **not**  override `alloc_words_rooted`, so it stays the test/embedding heap, not a collector; its
  `with_foreign_record_definitions`/`set_foreign_record_definitions` are the record-table hook behind
  `foreign_record_definition`, empty by default.)
- `context/` → the arena's two **host-driven reclaim tiers** over `ContextHeap` (`mod reclaim;`, its two
  report types re-exported from the crate root): Tier A `MinimalContext::heap_rewind(&mut self,
  HeapMark) -> Result<HeapRewind>` — a purely mechanical prefix rewind: drop every region strictly newer
  than the marked one (a real `Drop`, the only path that releases a carrier `mmap`), then `reset()` the
  survivor and advance it to `mark.used`, so it reads no word and costs `O(regions dropped)` —
  `HeapMark::EMPTY` doubles as the arena-reset door; and Tier B `MinimalContext::reclaim_heap(&mut self,
  impl FnMut(&mut beam_gc::Relocator<'_>) -> Result<()>) -> Result<Option<HeapReclaim>>` plus the
  `reclaim_heap_with_capacity(dest_capacity_words, ..)` twin — a Cheney copy of the live set the `forward`
  closure forwards, deposited **straight into a fresh destination** (reserved at the copy bound, `Σ
  used`; a post-copy memcpy is unsound because the relocator writes each copy's destination address into
  every copied child slot), after which the whole old grid is dropped and the survivor installed.
  `HeapRewind { regions_dropped, words_released, live_words, reserved_words, committed_words }` and
  `HeapReclaim { regions_dropped, source_words, live_words, words_reclaimed, objects_copied,
  reserved_words, committed_words }` are plain counts, never samples. Refusals are decided **before the
  first copy**, with the arena byte-identical: `BeamError::BadArg` for a stale/foreign/forward mark, the
  typed `heap_exhausted` for an undersized destination, `Ok(None)` (no state change, no epoch bump) for
  nothing worth doing; an `Err` from the host's own `forward` is unrecoverable and leaves the arena in
  its defined **empty** state. The caller contract (a violated liveness claim is a use-after-free, as in
  `beam_alloc::Heap::reset`; a forgotten root is dropped silently) and the `beam_engine → beam_gc` edge
  (wave 1 → wave 4, acyclic — `FromSet`/`LinearDest`/`Relocator` only) — `context/CONTEXT.md`
- `machine/` → `Machine` (registers + the `x_mut`/`fr_mut`/`stack_mut`/`with_roots`/`alloc_rooted`
  root-set hand-off, the two root-enumeration doors — `for_each_root`'s read-only pointer-word census and
  its **relocating twin `for_each_root_mut`**, the same root words handed over as `&mut [Eterm]` slices
  (one `with_roots` hand-off, so the parking/re-seating of the five loose terms happens in exactly one
  place and is restored even on the visitor's `Err`, `args_scratch` excluded for the dispatch-window
  reason) — frames, catch/unwind, run entries, `RunOutcome`, module contexts, driver,
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
  `jit/baseline/emit/CONTEXT.md` (its `bitstr_inline/` node owns the sixteen native `Bs*` shapes and the
  `bs_create_bin` construction sequences), `jit/baseline/inlineset/CONTEXT.md` (the authoritative inline
  set: which groups lower in-block, every run-time guard, and the three `bs_create_bin` construction
  routes with their structural refusals), `jit/cranelift/CONTEXT.md`, `jit/bench/CONTEXT.md`
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
A **fourth**, different kind sits beside them: `Machine::for_each_root_mut` (`machine/mod.rs`) hands the same root words over as relocating `&mut [Eterm]` slices, but it is a **host-driven relocation door**, not an admission point — it allocates nothing, collects nothing, runs only when the host itself decides to move terms (the arena's Tier-B `reclaim_heap` closure is its consumer, `context/CONTEXT.md`), and therefore never intervenes between an engine allocation and its result. Its read-only twin `Machine::for_each_root` censuses the same set as word copies.
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
(`Machine::loose_heap_terms`, `machine/mod.rs`). The engine cannot derive the predicate itself: the live
set spans the host's own stores, which the engine never sees, so the completeness claim is the host's — and
the crate's `beam_gc` edge is the arena's Tier-B reclaim only (`FromSet`/`LinearDest`/`Relocator`,
`Cargo.toml`, `context/reclaim.rs`), not a reservoir walk. An in-slice prune is also unsound for the accumulator **in flight** — the roots do not
name a `bs_init_writable` payload until its term is written back, while `retain_refc_binaries` prunes the
writable registry in lockstep (`offheap.rs:138-142`).
Every other engine→host call carries **no** root set, so the seams are **publication-free by
construction**: every term a seam consumes is re-derived from the machine's root set immediately before
the call and re-read after it, so no `Eterm` a seam could move is ever held in a Rust local across it.
Those calls, and what each re-derives:
- `ExecutionContext::resolve_local_fun` (`families::funs`' `call_fun`) — the `fun` target is re-read
  through `FunSource::read` (from `x(arity)` or the `call_fun2` operand) **after** the seam, and
  `terms::fun_free` (`terms/fun.rs`) borrows that post-seam block so the free-variable words are read
  from a live term; nothing allocation-capable lies between that read and the `set_x(arity + i, ..)`
  writes (`set_x` writes a register, not an admission point).
- `ExecutionContext::apply_fun_outcome` (the `Ok(None)` fallback of the same `call_fun`, whose default
  body calls `apply_fun`) — the fallback re-derives **both** the fun (`FunSource::read`) and the argument
  list (`register_args` over `x(0..arity)`) after the seam. A `FunOutcome::Suspend` writes nothing, so
  the parked instruction's operands stay readable from `x(arity)`/`x(0..arity)` for the resume's
  re-consultation. Pinned by `tests/fun_seam_rooting.rs` under a seam that collects.
- `ExecutionContext::resolve_external_call` (`families::funs::apply_target`) — the argument list is built
  **after** the seam declines, and the callee's atom words are re-read from `x(arity)`/`x(arity+1)` after
  `dispatch_bif_call` before the `undef` head item is built (`tests/fun_seam_rooting.rs`).
- `ExecutionContext::call_bif` (`interpreter/bif.rs`) — the marshalled arguments are **copies** living in
  the `EMPTY_ARGS` stack array (`exec_bif`/`call_bif_named`) or `Machine::args_scratch` (**taken out of
  the machine** for the whole `dispatch_wide` call and restored afterwards, so it is not even reachable
  from `Machine` while the terms are live); **neither is in any `RootSet`**, and neither needs to be: the
  window is **collection-free by construction** — `Machine::dispatch_bif_call` is **`&self`**, both
  rooted doors take `&mut self` plus a `RootSet` borrowed from this machine, and the marshalling's only
  allocation is the non-collecting `alloc_words`. A host must therefore never make BIF dispatch a
  collection door; `tests/bif_args_window.rs` pins it (with a live `test_heap` control proving the probe
  sees a real admission point), and `src/interpreter/bif.rs` pins that the scratch is left empty on both
  wide-dispatch exit paths.
**Engine-held live terms and the published root set.** The machine's loose heap terms
(`Machine::loose_heap_terms`) are **parked into the published root set** by `Machine::with_roots` for the
duration of every hand-off and re-seated from their slots afterwards (a `with_roots` fast path hands over an
unmodified `RootSet` when none is armed), so a collecting host relocates them with no host-side change.
The parked set is the two failed-clause argument latches
(`raise_function_clause_args`/`pending_function_clause_args`), **the pending exception's reason**
(`Machine::pending_exception`, `machine/catch.rs`) — the field the engine does not clear at landing, so it stays
readable for the rest of the run and is a live root at every door by construction — and the two supplied-trace
**term** latches (`pending_supplied_term`/`raise_supplied_term`).
`beam_rt`'s park publication of that reason through `Registers::extra_roots` (read back through
`Machine::pending_exception_mut`) is therefore reading an already-relocated word, not supplying the relocation.
The supplied-trace symbolic latches are metadata — `pending_supplied_trace`/`raise_supplied_trace` (`Option<StackTrace>`) —
and need no rooting. The two **term** latches `pending_supplied_term`/`raise_supplied_term` (`Option<Eterm>`) hold a cooked
`[{M,F,A,Loc}]` cons list outside the `RootSet`, so they are **parked** like every other loose term: `with_roots` puts them
at the stack tail and re-seats each field from the word the host wrote back
(`machine::root_tests::with_roots_re_seats_the_two_supplied_trace_term_latches_from_their_parked_slots`).
The complete set of `Eterm`-typed `Machine` fields is `x`/`fr`/`stack` (the `RootSet`), the five parked loose
terms above, `class_atoms: [Eterm; 3]`
(atom **immediates** only — never a heap pointer, so no rooting needed) and `args_scratch` (empty outside a dispatch, and the
wide-dispatch window that fills it is collection-free by construction — see the `ExecutionContext::call_bif` bullet above,
pinned by `tests/bif_args_window.rs`). Everything else on the machine is `u32`/`bool`/`usize`
metadata or an `Arc<Module>` handle, so `current_code`, `raise_snapshot`, `entry_call`, the frame and
catch-frame stacks and the symbolic supplied-trace latches carry no heap term. The other Rust-local vectors are
recorded in `../../beam_rt/M1_GAPS.md` and `../../beam_rt/src/context/CONTEXT.md`.

## Execution position (`Machine::current_code`) — the complete store set
The position has **two** writers plus the `reset` clear: `Machine::step_instruction_with_entry`
(`interpreter/dispatch.rs`) writes the field directly at every interpreted instruction's entry — the
instruction-granular store every suspension and raise relies on — and `Machine::set_current_code`
(`machine/location.rs`) is the accessor `src/jit/**` reaches; `Machine::reset` sets it back to
`CURRENT_CODE_NONE`. The field is **one packed `u64`** (`func << 32 | pc`, `pack_current_code`/
`unpack_current_code`, with `u64::MAX` and the `func == u32::MAX` sentinel as "no position"), so a store is a
single word write and every reader unpacks it.
Every store is an instruction boundary, but **not** every store happens before the thing it names runs:
- Interpreter: `Machine::drive` stores **before** the charge and the body, so a `reduce` refusal parks on
  the **not-yet-executed** instruction; then `step_instruction_with_entry` re-stores the same address.
- Native baseline tier, driver head (`drive` in `jit/baseline/mod.rs`): once per driver-loop iteration,
  in the order `set_current_code(func, pc)` → the `Machine::reductions` bump → `ExecutionContext::reduce(1)`
  → `code.enter`, so a refusal parks at the block entry with the block unexecuted.
- Native block prologue (`jit/baseline/emit/prologue.rs::emit_block_prologue`): the **block's entry** pc,
  stored **inline** (`emit_position_store`: `mov r10,[rdi+RUNTIME_CURRENT_CODE_DISP]`;
  `mov r11, movabs pack_current_code(func, pc)`; `mov [r10], r11` — three instructions, no helper call),
  or — when `Allocation::prologue_publishes_y` holds — through `jit_publish_y`, which makes the same store
  **and** re-publishes the frame's Y window in one call; so mid-block the position is a block entry and a
  resume from it **re-executes the block prefix**.
- Exact per-instruction helpers, each storing the instruction it is about to run: `jit_step_ext`,
  `jit_family` (shared with `jit_receive`, which is a thin wrapper) and `jit_test`, plus the frame/heap
  helpers `jit_deallocate`, `jit_return`, `jit_deallocate_return`, `jit_allocate`, `jit_allocate_heap`,
  `jit_test_heap`, `jit_trim` — all in `jit/baseline/runtime.rs`. `resume_in_block` stores the address it
  is about to resume at, in the driver loop head's own order; a `jit_step_one` deopt reaches the store
  through `step_instruction_with_entry`.
- Native `Bs*` arm (`jit/baseline/emit/family.rs::emit_bitstr`): the instruction's **own** pc, stored
  **inline before the helper call** through the same `emit_position_store` encoder the prologue uses
  (`emit_positioned_helper_call` places it after the `rdi = [rsp+RUNTIME_SLOT_DISP]` load and before the
  argument loads — three instructions, no call, zero extra `rt` fetch), exactly like the prologue. So
  `jit_bitstr` (`jit/baseline/bitstr.rs`) stores **nothing**: the position is emitted at compile time
  instead of re-derived from the helper's `func`/`pc` arguments on every execution.
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
Files inside a subtree with its own `CONTEXT.md` (`machine/`, `terms/`, `interpreter/`, `families/`, `jit/`, `context/`, `bench/`) are documented by that node's `CONTEXT.md` and not listed here.
- `context.rs` (3 585 lines) — the entire runtime seam: `ExecutionContext` + `Mailbox` + `RootSet` +
  the `MinimalContext` region heap (its census/mark doors included; the two reclaim tiers are the child
  node `context/`).
`exec_census.rs` (794) and `operand.rs` (465) are the largest top-level files after it; every other
`src/*.rs` is under 250.

## Per-module unit-test coverage
Every `src/**` module carries `#[cfg(test)]` unit tests beside the code they cover (the integration suites are the sibling node `../tests/`).
The exhaustive enumeration follows, one item per module (or per sub-suite where a module splits its tests):
`context` (hook dispatch, default-hook unsupported, the `alloc_words` arena pointer stability/aliasing, `apply`/`apply_fun` forwarding and their defaults, the apply hook — plus the arena census/mark/region-walk suite `heap_stats_is_empty_before_the_first_allocation`, `heap_stats_reports_every_region_and_the_frontier`, `used_words_is_cumulative_across_regions`, `heap_mark_reports_the_frontier_and_detects_a_replaced_region` and `for_each_heap_region_hands_only_the_written_prefix_oldest_first`, each over `ContextHeap::with_geometry`'s exact growth arithmetic); `context::reclaim` (the two tiers' **8** exact-number module tests over `ContextHeap::with_geometry`'s 4-/8-word reservations — `a_rewind_drops_every_later_region_and_keeps_the_words_below_the_mark`, `a_rewind_inside_one_region_only_moves_the_bump_pointer`, `an_epoch_matching_empty_mark_is_the_arena_reset_door`, `a_foreign_forward_or_stale_mark_is_refused_and_changes_nothing` (the only place a forward/replaced-region/foreign mark is fabricated, which is why the suite is a descendant module), `a_reclaim_collapses_the_arena_into_one_region_and_forwards_its_roots`, `a_destination_too_small_refuses_before_the_first_copy`, `a_reclaim_reports_none_when_there_is_nothing_to_do` and `an_error_from_the_forward_closure_leaves_the_arena_empty`); `machine` (the cross-cutting raise-funnel suite in `src/machine/tests.rs` — every raise kind propagated through a real run entry, a caught raise unchanged, and the **raise-time snapshot**: `throw_with_trace` captures the pre-unwind `(context, func, pc)` chain on **both** the caught and the uncaught path, an already-unwound caller is still named, `Machine::reset` clears it and a resume preserves it); `machine::root_tests` (the `Machine::for_each_root` pins — coverage of every rooted file plus the five loose terms incl. `with_roots_re_seats_the_two_supplied_trace_term_latches_from_their_parked_slots`, the immediate filter, and agreement with the `with_roots`/`RootSet` pointer-bearing word set — beside the **four `for_each_root_mut` parity pins**: `for_each_root_mut_enumerates_the_same_roots_as_for_each_root` (the same multiset of pointer-bearing words out of both doors), `for_each_root_mut_relocates_the_very_slots_for_each_root_reports` (a moving visitor rewrites the slots the census names) and `for_each_root_mut_writes_are_observed_by_for_each_root` (a write through the writable door is what the census then reports), plus `for_each_root_mut_excludes_args_scratch_and_restores_the_parking_on_error` (the shared `args_scratch` exclusion and the loose-term parking re-seated around a visitor's `Err`)); `machine::registers` (the x/FR files, their bounds and `reset`); `machine::frames` (frame push/pop/return, `trim`, Y-slot scoping and bounds, `ensure_heap`'s root-set forwarding, `resumption_addresses`, `raise_snapshot`); `machine::catch` (the catch-frame stack, the throw/unwind/landing paths, `pending_exception`/`pending_class`, and the `with_uncaught_frames` funnel — it attaches only to an empty raise, leaves a diagnostic unframed, and resolves each frame in its own module context); `machine::run` (`run_export`/`run_label`, `RunOutcome`, the blocked-receive fold and `resume_outcome`); `machine::drive` (the per-instruction reduction charge and the slice counter); `machine::location` (the `wait_deadline`/`timeout_delay` accessors over a literal and a Y-register delay); `predicates` (the `beam_term` boxed-term predicate wrappers, incl. native-record vs tuple); `terms` (every constructor↔reader round-trip through `MinimalContext` — incl. `make_map` across the 32-key flat/hashmap boundary and the one-reservation carve — raw-word layout proofs, both map readers over hand-built nodes, and the error cases: `make_flatmap` >32 / fun field >255 / binary >64 → `unsupported`/`SystemLimit`, wrong-type readers → `BadArg`); `terms::alloc_tests` (the **rooted-allocation** suite: a counting context proves the `*_rooted` builders fire the rooted hook only, a semi-space **moving** collector proves the reserve → read → write ordering, and a peak-word table contrasts the arena (which frees only through a host-invoked reclaim tier) with the collecting one); `operand` (read/write each supported kind incl. `FloatReg`↔FR file and out-of-range, `Literal` via `Machine::literal` and a missing one → typed error; `Label`/`List`/`Allocation` → unsupported); `interpreter` (the label map + resolution, and — in `src/interpreter/tests.rs` — the `NO_FAIL` guard raises: a failed `test`/`select_val` with `fail == NO_FAIL` raises class `error` with the bare atom `function_clause`, once through `step_instruction` and once through `run_label`, landing in a live `try` frame, while a module that cannot name the atom is the typed `unsupported_opcode: <site>: no 'function_clause' atom in the module`); `jit` (the roster invariants — interpreter first, `"baseline-jit"` always present, `"jit-cranelift"` iff the feature is on, the roster is a `Tier::ALL` prefix; `Tier::name`↔`from_name` round-trips + unknown/empty/miscased names ⇒ `None`; `run_with_tier` on the reused `slice.beam` fixture — `Interpreter` equals `run_export`, `BaselineJit` equals it too, `Cranelift` is a typed error); `jit::baseline` (block-target `encode_target`/`decode_target` round-trips and `HALT`/`ERROR` sentinel distinctness, the `JitRuntime` field offsets generated code relies on, that cached slots index the x-register array rather than the `Machine` struct, the cached/directly-addressed register allocation and its bound, and the `constant_word`/`x_register` operand forms); `jit::baseline::emit::link` (the direct cross-block `jmp` linkage — an emitted `jmp rel32` landing on the successor's compiled entry, the `rt` reload on a linked exit, and the cached-slot spill); `jit::baseline::emit::test` (the `test_plan` inline/refuse decision: the word tests plus the boxed-term `TestKind`s it lowers, and the refusals — `NO_FAIL`, an unresolved label, an `arity == 0` tagged tuple, a missing `true`/`false` for `is_boolean`); `jit::cache` (the two-part-keyed compiled-block cache — `module_fingerprint` stability across reloads, same-name/different-content non-collision, the namespace partition `two_loads_of_one_beam_are_two_namespaces_and_two_entries`, `get_or_compile` reuse, and `clear`); `jit::emitter` (the encoder is proven by *executing* what it emits: `add`, a six-argument sum (`r8`/`r9`, `r12`/`rsp` addressing), `lea`, `cmp`+`jcc`, an indirect `call_r`, label patching, `seal` flipping `/proc/self/maps` to `r-xp`, and the error paths); `jit::tiering` (the cold/warm/hot boundaries, the `hot ≥ baseline` clamp, a zero baseline threshold, equal thresholds skipping the baseline rung, `Default`, `is_hot`, and that `select` never names a tier outside `available_tiers`); `jit::cranelift` (the feature-gated backend: tier name/availability, one native entry per non-empty function, the empty module, and a **direct call into a compiled entry** proving native execution); `families::*` (per-family unit tests, plus the `families::dispatch` tests in `families/mod.rs` — `families::{bitstrings/{create,matching,raw,run_tests},exceptions,floats,funs,maps,receive,records}`, and the split families group their suites further: `families::exceptions::tests::{handlers,raising,stacktrace,end_to_end}` (the `stacktrace` sub-suite cooks the non-`NIL` arm from the **raise-time snapshot**, incl. an already-unwound caller and a frame resolving in its own registered module context) and `families::records::tests::{native,elements,update,access,field,foreign_create}`; `families::funs::tests` also owns the four `apply`/`apply_last` cross-module-seam tests — the `apply` `Step::Enter` + caller return address, the cross-module receive suspend/resume, the `Ok(None)` sync-path fallback, and `apply_last` popping its frame first; `records::tests::update` keeps the update paths plus the create form's no-host typed errors, while `records::tests::foreign_create` (14 tests) is the create form against a **host record table** — the `foreign_record_definition` seam's exported, non-exported and `Ok(None)` branches (the `badrecord` one on both the `NO_FAIL` raise and a live `fail` label), the `{SelfModule,Name}` export gate vs the atom-`Id` local form, `badfield` outranking `no_value`, the first-absent-field `no_value`, `Some(small 0)` vs an absent default, the unnameable-name rows and interpreter/baseline-tier parity); `families::bitstrings::run_tests` is the family's end-to-end + oracle suite (always through `Machine::run_label`/`run_export`, never a family helper): a non-byte-aligned refc subject read bit-for-bit through `bs_start_match3`/`bs_get_tail`/`bs_get_integer2`, an `append` segment in a **non-first** position, and a live `erlc`/`erl` cross-check of an inline module ; `families::maps::tests` pins the read instructions' `NO_FAIL` raise (a missing key with `fail == NO_FAIL` raises the bare atom `function_clause`, caught and uncaught, while a module that cannot name the atom is the typed `unsupported_opcode: maps: no 'function_clause' atom in the module`), beside the unchanged `{badmap,_}`/`{badkey,_}` write-path raises); `exec_census` (the measurement instrument's **8** unit tests: off by default and `reset` zeroing the counts, a `Test` instruction keyed by its kind, `first_error` retaining only the first text, the TSV dump's shape and its section-name sanitisation, the executed count taken through the interpreter funnel, a raising instruction counted as **executed *and* errored**, and per-thread isolation); and the sibling integration suite `../tests/heap_reclaim.rs` re-proves both tiers through the **public** doors at the **production** geometry (4 MiB reservation / 64 KiB commit, a second region only past a bulk request) in **7** rows — `a_public_rewind_drops_the_grown_regions_and_keeps_the_words_below_the_mark`, `a_foreign_or_pre_rewind_mark_is_refused_and_leaves_the_arena_byte_identical`, `an_epoch_matching_empty_mark_resets_the_arena_and_the_emptied_arena_allocates_again` (the reset door plus a re-allocation afterwards), `a_public_reclaim_collapses_the_arena_and_forwards_its_roots`, `a_capacity_below_the_live_volume_is_refused_before_the_first_copy`, `a_reclaim_reports_none_when_there_is_nothing_to_do` and `a_machine_rooted_reclaim_forwards_the_machines_own_root_set` (the exact driver call pattern: a real `Machine` root set relocated through `for_each_root_mut`) — `../tests/CONTEXT.md`.
## Root-set contract: the two admission doors and the reserve -> read -> write discipline (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **Root-set contract made concrete, with both admission doors defaulted.** The primary admission point is the defaulted `ensure_heap_full(heap_need, roots)` over `RootSet`, which `Machine::ensure_heap` calls so the context gets the exact GC root set (`x[..used_x_extent()]` + the whole stack + `fr[..used_fr_extent()]`) and a Stage-4 collector can move terms in place without frame liveness maps; its default forwards to the frozen narrow `ensure_heap(heap_need, live, x_regs, stack)` with the FR file discarded, so an existing host is byte-identical. The **second** admission point is `alloc_words_rooted(n, roots)` over `RootSet` (`x`/`stack`/`fr`, all mutable, plus the `live` hint) -- the root hand-off for *term construction*, because `alloc_words` itself has neither an admission point nor roots; its default discards the roots and forwards to `alloc_words`, so an existing host is byte-identical. **Every in-crate term-construction arm routes through the second door** via the `terms::*_rooted` builders, which follow the frozen **reserve → read → write** discipline (`reserve` the block first with *exactly one* rooted allocation, then read operands from the machine's post-collection state, then write the header/element words); `src/terms/slack.rs` is the reserve seam and `src/terms/sink.rs` the `Sink` shape writer a multi-node build is carved from. `terms` is the crate's dedicated raw-heap accessor module (the per-family `raw` modules and the JIT encoder are the documented layout exceptions, listed in `CONTEXT.md`); every in-crate construction **arm** goes through a `*_rooted` twin, while the classic unrooted builders (`terms/{construct,number,map,fun,sink}.rs`, plus the four `&self` execution-position accessors in `src/machine/location.rs` that cannot lend `&mut` roots) are the host/test API and are reachable in production only from a caller that already cannot collect.

## Who owns the collecting host (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **The collecting host is `beam_rt`'s side, not an engine gap.** `MinimalContext`'s reserved-heap-region arena grows without bound and never reclaims on its own — only its two host-invoked tiers give words back, and no engine path calls them (`context/CONTEXT.md`) — and `test_heap`/`allocate_heap` reach the full-root admission point (`ensure_heap_full`, a no-op there -- its default forwards to the no-op `ensure_heap`). The engine's own half is landed: **every** in-crate term-construction arm allocates through the rooted door (`alloc_words_rooted` / `Machine::alloc_rooted` / the `terms::*_rooted` builders), under the frozen reserve → read → write ordering, proved under a mock **moving** collector by `src/terms/alloc_tests.rs` (the rooted-vs-unrooted peak/retained live-words table) and `tests/rooted_alloc.rs`, with the root hand-off pinned by `tests/gc_roots.rs`. **`beam_rt`, the production heap, overrides both rooted doors** (`beam_rt/src/context/execution.rs`) -- it publishes the whole `RootSet` it is handed (the used X/FR prefixes plus the whole stack), runs `beam_gc` over it, writes the moved terms back, and declines a collection whose machine root reaches a container in a region the collector does not scan -- so an ordinary `beam_rt` run collects at the engine's heap-admission point, conservatively gated (`beam_rt/src/context/gc.rs`: a process is pinned once a live float register is seen, halted on a collection error, and a global valve disables collection once an escaped runtime store holds an off-heap term). A host has **no** rooting obligation for the BIF argument window: the words in `Machine::args_scratch`/the `exec_bif` stack array are unreachable from any door (the scratch is `mem::take`n out for the whole dispatch) and the window is collection-free by construction (`Machine::dispatch_bif_call` is `&self`; both rooted doors need `&mut self` plus a `RootSet` borrowed from this machine; the marshalling's only allocation is the non-collecting `alloc_words`), pinned by `tests/bif_args_window.rs` — its obligation is the inverse, never to make BIF dispatch a collection door. The machine's own five loose heap terms -- the two failed-clause latches, `Machine.pending_exception`'s reason (`src/machine/catch.rs`, reachable mutably through `Machine::pending_exception_mut`) and the two supplied-trace term latches -- need no host enumeration: `Machine::with_roots` parks them at the stack tail in the `RootSet` it hands over and re-seats each field from the word the host wrote back. Detail: `src/CONTEXT.md` (`## GC publication…`).

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
Every tier reports and resumes a suspend: the interpreter's driver folds `Step::Suspend` into `RunOutcome::Suspend` (`src/machine/drive.rs`), the native baseline tier carries it out of a compiled block as the `SUSPEND` (`-5`) block-ABI word with its address in the `JitRuntime::suspend` side channel, and `Tier::Cranelift` re-enters through the **shared no-reset resume seam** `jit::baseline::run_native_resume_outcome` with the artifact the retained store (`jit/cranelift/cache.rs`) kept mapped across the park: an address `CompiledFunctions::is_enterable` accepts (a block start or a non-tail-call continuation) runs compiled code natively, every other address -- most visibly a `wait`/`wait_timeout` suspension `pc` -- advances by the driver's **per-instruction interpreter step**, and a park inside a non-zero module context is landed by the context switch with that callee running per instruction (this tier holds no cross-module blocks). No tier change on a resume.
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
