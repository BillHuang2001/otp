# `beam_engine/src` — Engine Source Tree

## Intent
The implementation body of `beam_engine`: the typed-IR execution engine (dispatch loop, module-context-aware driver, the nine instruction families, the term builders, the JIT tier seam).
This node owns `src/**` only; the crate's frozen public API, the cross-crate seams and the exhaustive execution semantics are one level up — read `../CONTEXT.md` first, then `../INSTRUCTION_SURFACE.md`.
Child nodes own their subsystem detail (`machine/`, `interpreter/`, `families/`, `jit/`, `terms/`, `context/`, `bench/`); measured figures are `../MEASURED.md`/`../PERF.md`, the test enumeration `../TEST_NOTES.md`.

## Cross-crate facts a `src/**` change must respect
### `beam_engine::terms` is a PUBLIC module (consumed by `beam_rt` and `tests/**`)
The **classic, unrooted** builders are the public names and their signatures are frozen: `make_tuple(ctx, &[Eterm])`, `make_cons(ctx, head, tail)`, `make_list(ctx, &[Eterm])`, `badmatch_reason(ctx, module, value)`, `make_heap_binary(ctx, bytes)`, `make_float(ctx, v)`, `make_integer(ctx, v)`, `make_flatmap(ctx, pairs)`, `make_map(ctx, pairs, atoms)`, `make_fun(ctx, arity, uniq, index, free)`; they allocate through `ExecutionContext::alloc_words`.
Do **not** rename or re-signature them — Rust has no overloading, so a signature change silently breaks `beam_rt` and this crate's own tests.
The **rooted** twins are what every in-crate instruction arm and family calls: `make_tuple_rooted`, `make_cons_rooted`, `make_list_rooted`, `badmatch_reason_rooted`, `make_heap_binary_rooted`, `make_float_rooted`, `make_integer_rooted`, `make_flatmap_rooted`, `make_map_rooted`, `make_fun_rooted`.

### `machine::RunOutcome` is matched EXHAUSTIVELY outside this crate
`beam_rt/src/boot/drive.rs` and `beam_rt/src/runner/run.rs` match it, and it is exactly `{ Returned(Eterm), Wait { func, pc }, Suspend { func, pc } }` — there is no yield/pre-emption variant.
`Wait` is released by the mailbox (the `loop_rec` ran and reports the resume address); `Suspend` is released by the host re-issuing the BIF (the instruction did not run, so the address is the instruction itself).
The plain `run_export`/`run_label` (and the JIT backends' `run_export`) fold `Wait` into `error::blocked()` and `Suspend` into `error::suspended()`, never into a substituted value.
`Step` (`step.rs`) is `pub(crate)` and is exactly `Next | Goto { func, pc } | Enter { context, func, pc } | Wait { func, pc } | Suspend { func, pc } | Done`; a body returns the `Step` and never mutates the loop's `func`/`pc`/context.
A raise is **not** a `Step`: it stays an `Err(BeamError)`, or the exception layer folds it into a `Goto`/`Enter` to a live catch handler.

### Reduction / pre-emption
`ExecutionContext::reduce(n)` is a **defaulted no-op** hook, charged **once per interpreted instruction, before the instruction's effect**, by `Machine::drive`, and **once per driver-loop iteration (before `code.enter`)** by the native baseline driver — so a compiled loop is pre-emptible at block granularity and a natively linked tail-call chain is priced once per round-trip.
A host pre-empts by returning its own typed error; the instruction stays **un-applied** with the machine parked at its `(func, pc)`, so `Machine::resume_outcome` applies it **exactly once**.
`wants_reduce` latches per run (a host implementing `reduce` MUST stay `true`); observation is `Machine::reductions()`/`reset_reductions()`.
There is no `RunOutcome::Yield` and no `ReduceOutcome`.

### `Step::Enter` crosses the module boundary on every tier
A raise landing in another context's catch frame, a resolved cross-module `call_ext`, and a cross-module `return` onto a caller-context continuation all cross as `Step::Enter`; the driver switches the context and re-binds the literal area there.
The **native baseline tier** carries it out as the `ENTER` block-ABI word plus the same `Machine::switch_context`/literal-rebind path.
A **seal-time cross-module link is unsound** (each `CompiledModule` owns a separate executable mapping, `rel32` fixups are buffer-local, the entry module is baked into `JitRuntime`), so the transfer is a driver round-trip by design.

### The in-machine BIF door is latched by BOTH drivers
`wants_bif_in_machine()` (default `false`) is latched once per run into `Machine::bif_in_machine_on` by `Machine::drive` and by the native baseline driver; every BIF dispatch funnels through `Machine::dispatch_bif_call` (`interpreter/bif.rs`), which takes exactly one `call_bif_in_machine_outcome`/`call_bif_outcome` per run.
The run's **entry module** (context `0`) is threaded from the driver through `step_instruction_with_entry` → `exec_bif`/`call_bif_named`/`dispatch_wide`/`families::dispatch` → `dispatch_bif_call`, and on the native tier through `JitRuntime.entry_module` (a field generated code never reads).
A host that opts in must be reached on **every** tier; `tests/bif_in_machine.rs` is the tier-parity gate.

### The local-call seam memoizes the host verdict, per callee function index, per run
`Machine::local_call_seam` (`interpreter/dispatch.rs`) names the same-module callee of a local `call`/`call_only`/`call_last` and may ask `ExecutionContext::resolve_local_call(module, function, arity)`.
`LocalVerdicts` (`machine/local_callee.rs`) keeps identity + verdict in one `Cell` slot per `Module::functions` entry, in the executing module context, so the host is consulted **once per distinct callee per run** and an un-nameable callee zero times — caching is the faithful choice because ERTS charges nothing for an intra-module call.
Three doors invalidate: `Machine::reset`, the BIF funnel when it dispatches `erlang:load_nif/2` (it only **arms** a flag — `dispatch_bif_call` is `&self`), and the public `Machine::invalidate_local_call_verdicts`.
A verdict may **never** be baked into compiled code: `JitBackend::compile` gets no `ExecutionContext` and the cross-run `CodeCache` is content-keyed and shared.

### The context seam (`context.rs`)
`pub trait ExecutionContext` is `&mut self` methods over `Eterm` (signatures are authoritative in the file), and **only `call_bif` and `ensure_heap` are required** — every other method has a default body, so a minimal host compiles by implementing nothing else.
`Mailbox` is the other trait: exactly nine **required** `Result`-returning methods — `send_message`, `receive_next` (`None` = scan exhausted), `receive_skip`, `receive_remove`, `receive_reset`, `receive_marker_reserve`/`_bind`/`_use`/`_clear` — mirrored on `ExecutionContext` as **defaulted** methods answering `error::unsupported(...)`, so a host that installs no mailbox still compiles.
The defaulted groups: the outcome-aware BIF twins (each forwards its value-returning door wrapped as `Return`); `wants_bif_in_machine` (`false`) and `call_bif_in_machine`; `ensure_heap_full` (→ the narrow `ensure_heap`, FR file discarded); `heap_available`; `alloc_words` (typed `unsupported`) and `alloc_words_rooted` (→ `alloc_words`, roots discarded); `apply` (→ `call_bif`), `apply_fun` (typed `unsupported`) and `apply_fun_outcome` (→ `apply_fun` as `Return`); `send_message_with_off_heap` (→ `send_message`); `resolve_external_call`/`resolve_local_fun`/`foreign_record_definition` (`Ok(None)`), `resolve_local_call` (`false`), `atom_name` (`None`), `atoms` (`&module.atom_table`), `reduce` (no-op), `wants_reduce`/`trace_enabled` (`false`), `trace_event` (no-op).
Every one of those defaults is what turns the corresponding engine path into a **typed** error/absence, never a wrong result.
`resolve_external_call` must hand back the **shared** `Arc<Module>` (machine module contexts dedup by `Arc::ptr_eq`) with `entry_label` the export label after `func_info`.
`apply_fun` is consulted only after `resolve_local_fun` declines, and enters with `x(0..arity)` = arguments first, free variables after (C's `call_fun` order) — never `free ++ args`.

### `unsafe` confinement
The crate is `#![deny(unsafe_code)]` + `#![warn(missing_docs)]`; every exception carries a module-local `#![allow(unsafe_code)]` and every `unsafe` block a `// SAFETY:` rationale.
This node's exception files: `operand.rs`, `predicates.rs`, `context.rs` (the region heap's bump seam) and `context/reclaim.rs` (the same bump advance plus the `FromSet`/`LinearDest` construction).
Child-owned exception modules: `terms/**`, `families/{bitstrings,records}/{raw,mod}.rs`, `machine/registers.rs`, `jit/emitter/**`, `jit/baseline/**`, feature-gated `jit/cranelift/**` — the authoritative set is `grep -rn 'allow(unsafe_code)' src`.
Errors are `beam_common::{Result, BeamError}` only; an unsupported case is a typed error, never a panic and never a silently wrong result.

### A term produced in a `MinimalContext` arena dies with that context
The arena owns the only copy of the words, a `Machine` owns no heap, and dropping the context returns the carrier blocks to `beam_alloc`'s PROCESS-GLOBAL segment pool, where the next arena writes over them.
Reading such a term afterwards is a use-after-free that surfaces as a **wrong value** or the `beam_term::ops::class: invalid boxed header subtag <n>` abort — never a typed error, and only intermittently.
A test comparing terms from two runs must keep **both** hosts alive to the comparison, or compare inside each run's own scope.

## Routing Table
Every root-level `src/*.rs` file has an entry here; subtree detail is the child node's.
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`.
- `context.rs` → **the whole runtime seam**: `ExecutionContext` (the BIF doors and their outcome-aware twins; the GC doors `ensure_heap`/`ensure_heap_full`/`alloc_words`/`alloc_words_rooted`; `heap_available`; `apply`/`apply_fun`/`apply_fun_outcome`; `send_message_with_off_heap`; the resolvers `resolve_external_call`/`resolve_local_fun`/`resolve_local_call`/`atom_name`/`atoms`; `foreign_record_definition`; `reduce`/`wants_reduce`; `trace_enabled`/`trace_event`), `Mailbox` (nine methods mirrored as defaulted `ExecutionContext` methods), `RootSet`, `MinimalContext` + its `ContextHeap` arena. Signatures are authoritative in the file; the arena's reclaim tiers are the child node `context/`.
  Arena geometry: regions are `beam_alloc::ReservedHeap` carriers (anonymous `mmap`, address space only) grown by `push` — the frontier is grown **in place** to exactly `used + n`, else a fresh region with `reserved = prev.reserved*2` floored at `HEAP_RESERVE_WORDS` (**4 MiB**) and `committed = HEAP_COMMIT_WORDS` (**64 KiB**) raised to at least `n` and clamped to `reserved`; every handed-out pointer is therefore stable.
  Measurement/mark doors: `heap_stats() -> ContextHeapStats` (`used_words` is **cumulative demand** = `live_words + released_words`, never live memory), `heap_mark() -> HeapMark`, the typed `heap_mark_matches(HeapMark) -> bool` (a stale, forward or pre-reclaim mark is `false`), and the census walk `for_each_heap_region` (each region's written prefix, oldest first — raw words: do not interpret them as terms or keep the slice).
  A mark is a **precondition check, never a permission**, and no path the crate takes ever frees, truncates or reuses a region — an arena whose host calls no tier behaves as one that could never reclaim.
  `MinimalContext` does **not** override `alloc_words_rooted`, so it is the test/embedding heap, not a collector; `with_foreign_record_definitions`/`set_foreign_record_definitions` back `foreign_record_definition` (empty by default).
- `context/` → the arena's two **host-driven reclaim tiers**: Tier A `heap_rewind(HeapMark) -> Result<HeapRewind>` (a mechanical prefix rewind — the only path that releases a carrier `mmap`) and Tier B `reclaim_heap(forward)`/`reclaim_heap_with_capacity` (a Cheney copy deposited straight into a fresh destination; a post-copy memcpy would be unsound because the relocator writes the destination address into every copied child slot). Every refusal is decided **before the first copy** and leaves the arena byte-identical; contracts and report types: `context/CONTEXT.md`.
- `supplied_stacktrace.rs` → `encode_supplied_stacktrace(module, ctx, frames)` and its `Machine::` twin: cooks symbolic frames into the oracle's `[{Module,Function,Arity,Location}]` list (`Location` = `[{file,File},{line,N}]`, else `[]`) as ONE rooted reservation, with atoms resolved by **lookup only** from the passed `module`'s table — an unresolvable name is `error::unsupported`.
- `step.rs` → the `pub(crate)` `Step` protocol (variants above).
- `operand.rs` → operand read/write (`FloatReg`↔FR file, `Literal` via `Machine::literal`; `Label`/`List`/`Allocation` → typed unsupported).
- `predicates.rs` → the `beam_term` boxed-term predicate wrappers, including native-record vs tuple.
- `error.rs` → constructors over `beam_common::BeamError`, never a crate-local enum: `unsupported(what)` (a `Raise` whose reason atom is `unsupported_opcode: <what>`), `bad_register(n)`/`unknown_label(l)` (`BadArg`), `blocked()` and `suspended()` (the two non-blocking-run diagnostics, each the single definition of its reason string), plus the typed `bad_fun()`/`undefined_lambda()` markers (each a `Raise` carrying a `()` live-reason payload, so a user `erlang:error(badfun)`/`(undef)` is never expanded into `{badfun,Fun}`) with their `is_bad_fun`/`is_undefined_lambda` discriminators.
- `generic.rs` → a residual `Instr::Generic` opcode reported as `error::unsupported(op.name())` — never a panic.
- `exec_census.rs` → the dynamic per-variant execution census (below).
- `machine/` → `Machine` (the two register files, frames, catch/unwind, run entries, `RunOutcome`, module contexts, driver, reduction counter, position introspection) and the root doors; see `machine/CONTEXT.md`.
- `interpreter/` → the dispatch loop, the BIF arm/raise, `TestKind`, A.8 trace emission; see `interpreter/CONTEXT.md`.
- `terms/` → the crate's only raw-heap builder/reader carve (`reserve`/`Slack`/`Sink`, the classic + `*_rooted` builders, `map`/`fun`/`number`); see `terms/CONTEXT.md`.
- `families/` → the `dispatch` seam — `exec(...) -> Result<Option<Step>>`, where `Ok(Some(step))` means executed, `Ok(None)` means not this family's variant and `Err` means it raised — plus the nine owning families (`tuples`, `lists`, `receive`, `funs`, `records`, `exceptions`, `floats`, `maps`, `bitstrings`); ownership is variant-complete, and an unclaimed or declined variant falls to `error::unsupported("widened opcode not yet executed: <ops.tab name>")`; see `families/CONTEXT.md` and its children.
- `jit/` → the tier seam (`Tier`/`JitBackend`), the cross-run `CodeCache`, the emitter, the native baseline tier, feature-gated `cranelift/`, the in-process bench harness; see `jit/CONTEXT.md`, `jit/baseline/CONTEXT.md`, `jit/baseline/emit/CONTEXT.md`, `jit/baseline/inlineset/CONTEXT.md`, `jit/cranelift/CONTEXT.md`, `jit/bench/CONTEXT.md`.
- `bench/` → the `#[cfg(test)]`-only in-process foreign-workload decomposition harness; see `bench/CONTEXT.md`.

## Dynamic execution census (`exec_census`)
A **public** module (not `#[cfg(test)]`, following the `jit::bench` precedent): per-`Instr`-variant executed/error counts keyed by the single attribution `exec_census::variant_name` (a `Test` by its kind), plus each variant's first error text. Surface: `enable`/`disable`/`set_enabled`/`is_enabled`/`reset`/`snapshot() -> Snapshot`/`write_dump(section)`.
The only recording site is `Machine::step_instruction_inner` (shared by the interpreter's `drive` and the native tiers' deopt helper): `on_instruction` before the instruction's effect, `on_error` only on an `Err(BeamError)`. Drivers latch `Machine::census_on` once per run, so a disabled census costs one load and a branch, zero thread-local accesses per instruction, and a counted run is byte-identical.
Blind spots, honestly: a raise folded into a `Step` by a live in-machine `try`/`catch` counts as executed but not errored, and a **natively-inlined** JIT instruction never reaches the funnel (`Tier::Interpreter` is the fully covered tier).
`write_dump` writes `std::env::temp_dir()/beam_engine_exec_census_<section>.tsv` (any section character other than alphanumeric/`-`/`_` becomes `_`; a write failure reports on stderr and yields `None`).
Measured rankings and the corpus-scale driver belong to `../MEASURED.md` and `../tests/dynamic_census.rs`.

## GC publication: which `src/**` engine→host calls can be intervened on
The engine hands a host its root set through **exactly three** doors, all under `src/machine/`: `Machine::with_roots(live, f)` (the `RootSet` carrier and the only hand-off body), `Machine::alloc_rooted(n, roots)` (→ `alloc_words_rooted`) and `Machine::ensure_heap(heap_need, live, ctx)` (→ `ensure_heap_full`).
A **fourth**, different kind sits beside them: `Machine::for_each_root_mut` hands the same words over as relocating `&mut [Eterm]` slices, but it is a **host-driven relocation door**, not an admission point — it allocates nothing, collects nothing, and runs only when the host itself decides to move terms (the arena's Tier-B closure).
Its read-only twin `Machine::for_each_root` censuses exactly what `with_roots` hands over (the used X extent, the whole stack, the used FR extent, and the five loose terms), skipping `IMMED1`; both doors are one `with_roots` hand-off so they cannot disagree, and `args_scratch` is excluded from both.
`with_roots` hands over what the run has **used**: `&mut x[..used_x_extent]`, the whole `&mut stack`, `&mut fr[..used_fr_extent]`, where the extent is `max(mark, dirty)` clamped to the file (`machine::registers::used_extent`, the one rule the hand-off and `Machine::reset`'s refill share). Every slot at or beyond the extent is `NIL` by the refill invariant and is not a root, and a host physically cannot write outside the slices it receives.
`live` is **only a hint** (`RootSet::live_len()` clamps it to the handed slice): the two heap doors pass the instruction's own `Live` operand verbatim, while **every** `alloc_rooted` caller passes `RootSet::ALL_LIVE`. The engine computes no liveness of its own.
Call sites: `alloc_words_rooted` has one production caller — `terms::reserve` (`terms/slack.rs`), hence every `*_rooted` builder, per built term. `ensure_heap_full` is reached only from the interpreter's `Instr::TestHeap`/`Instr::AllocateHeap` arms plus their baseline-JIT twins `jit_test_heap`/`jit_allocate_heap`, so it is a **per-`test_heap`** door — `Instr::Allocate` pushes its frame with **no** admission, and `gc_bif`/`bif`/`bif0`/`call_ext*` leave term allocation to the host's `call_bif`.
`ExecutionContext::heap_available` has **no** call site anywhere in the crate (introspection only).
Every other engine→host call carries no root set and is publication-free by construction: the term it consumes is re-derived from the machine's roots immediately before the call and re-read after it, so no `Eterm` a seam could move is held in a Rust local across it.
The BIF argument window is the sharpest instance: the marshalled arguments live in `exec_bif`'s stack array or in `Machine::args_scratch` (taken out of the machine for the whole wide dispatch), neither is in any `RootSet`, and the window is **collection-free by construction** — `Machine::dispatch_bif_call` is `&self` while both rooted doors need `&mut self` plus a `RootSet` borrowed from this machine, and the marshalling's only allocation is the non-collecting `alloc_words`. A host must therefore **never** make BIF dispatch a collection door; `tests/bif_args_window.rs` pins it, and `src/interpreter/bif.rs` pins that the scratch is left empty on both wide-dispatch exit paths.
**The off-heap reservoir is on no door at all:** `Machine::refc_binaries` is reachable only as `&self`, while `retain_refc_binaries` takes `&mut self` and so cannot run while a `RootSet` borrowed from this machine is alive. The engine cannot derive the prune predicate itself (the live set spans stores it never sees), so the completeness claim is the host's.
The machine's five loose heap terms are **parked into the published root set** by `with_roots` for the duration of every hand-off and re-seated from their slots afterwards (`LOOSE_HEAP_TERMS = 5`): the two failed-clause argument latches, the pending exception's reason, and the two supplied-trace **term** latches.
The rest of the `Eterm`-typed machine surface needs no rooting: `class_atoms` holds atom immediates only, and `args_scratch` is empty outside a dispatch. The two supplied-trace **symbolic** latches are metadata.
Host-side detail: `machine/CONTEXT.md` and `../tests/CONTEXT.md`.

## Execution position (`Machine::current_code`) — the complete store set
The position is **one packed `u64`** (`func << 32 | pc`) with sentinels for "no position", so a store is one word write and every reader unpacks it.
Two writers plus the `reset` clear: `Machine::step_instruction_with_entry` (`interpreter/dispatch.rs`) writes it at every interpreted instruction's entry — the instruction-granular store every suspension and raise relies on — and `Machine::set_current_code` (`machine/location.rs`) is the accessor `src/jit/**` reaches.
Every store is an instruction boundary, but not every store happens before the thing it names runs.
Interpreter: `Machine::drive` stores **before** the charge and the body, so a `reduce` refusal parks on the not-yet-executed instruction.
Native baseline driver head: once per iteration, in the order `set_current_code` → the reduction bump → `reduce(1)` → `code.enter`.
Native block prologue: the **block's entry** pc, stored **inline** (three instructions, no helper call) or through `jit_publish_y` when the block also re-publishes its Y window — so mid-block the position is a block entry and a resume from it **re-executes the block prefix**.
Exact per-instruction helpers (each storing the instruction it is about to run): `jit_step_ext`, `jit_family` (`jit_receive` wraps it), `jit_test`, and the frame/heap helpers `jit_deallocate`, `jit_return`, `jit_deallocate_return`, `jit_allocate`, `jit_allocate_heap`, `jit_test_heap`, `jit_trim`.
The native `Bs*` arm stores the instruction's **own** pc inline before its helper call, so `jit_bitstr` itself stores nothing.
Seam routes that store **NOTHING**: `jit_ext_call`, `jit_local_call`/`probe_local_call`, `jit_bif_value` and `jit_tail_call` — a host hook reached through them runs with a stale (block-entry-or-earlier) position, which is why `run_baseline_cached_resume_outcome` must not assume an exact instruction position on a suspended frame.
Emitter detail: `jit/baseline/CONTEXT.md`, `jit/baseline/emit/CONTEXT.md`.

## Root-set contract: the two admission doors and the reserve -> read -> write discipline
**No frame liveness maps exist.** A collecting host must relocate every term reachable from `RootSet { x, stack, fr, live }` — the used X prefix, **the whole Erlang stack**, the used FR prefix — writing the moved terms back, and **nothing outside the root set survives**; a raw `*mut Eterm` from a previous allocation is dead after the next collecting allocation.
The primary admission point is the defaulted `ensure_heap_full(heap_need, roots)`, which `Machine::ensure_heap` calls so the host gets the exact root set; its default forwards to the frozen narrow `ensure_heap(heap_need, live, x_regs, stack)` with the FR file discarded, so an existing host stays byte-identical.
The **second** admission point is defaulted `alloc_words_rooted(n, roots)` — the root hand-off for *term construction*, because `alloc_words` itself has neither an admission point nor roots; its default discards the roots and forwards to `alloc_words`.
**Every in-crate term-construction arm routes through the second door** via the `terms::*_rooted` builders, under the frozen **reserve → read → write** discipline: reserve the block with exactly one rooted allocation, then read operands from the machine's post-collection state, then write the header/element words (`terms::reserve` is the seam, `terms::Sink` the shape writer for a multi-node build).
The one unrooted runtime allocation is a non-small `Operand::Integer`/`Character` operand; the classic builders are the host/test API, reachable in production only from a caller that already cannot collect.
`RootSet::ALL_LIVE` is always safe; over-approximating a root set is always safe.
The engine makes **no collection decision** (`arm_gc`/`should_collect`/`collection_debt` do not occur in this crate): latch, occupancy test and collector are the host's, and the engine's only capacity input is the `heap_need` it passes.
Detail: `machine/CONTEXT.md`, `terms/CONTEXT.md`, `context/CONTEXT.md`.

## Who owns the collecting host
`beam_rt` — the production heap — overrides both rooted doors (`beam_rt/src/context/execution.rs`): it publishes the `RootSet` it is handed, runs `beam_gc` over it, writes moved terms back, and declines a collection whose machine root reaches a container in an unscanned region; a plain run therefore collects at the engine's heap-admission point, conservatively gated.
`MinimalContext`'s arena never reclaims on its own and no engine path calls its tiers, so the engine's own half of the contract is complete and every remaining adoption is host-side.
The engine's evidence that the root discipline is right: a mock **moving** collector over the rooted builders (`terms/alloc_tests.rs`), the root hand-off (`tests/gc_roots.rs`, `src/machine/root_tests.rs`) and the BIF-window pin (`tests/bif_args_window.rs`).

## The comparison atom-namespace seam (`ExecutionContext::atoms`)
`fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable` (default body `&module.atom_table`) is the namespace every **comparison** resolves names through: the six comparison kinds and `is_boolean` (`interpreter/test.rs`), `Instr::SelectVal` (`interpreter/dispatch.rs`), the baseline tier's `evaluate_comparison` and the map key ordering/lookup.
It is also the namespace every **raise** resolves its reason/class atoms through, via `machine::catch::namespace_atom(ctx, module, name)`.
The default is the module's **load-time snapshot**; a host owning ONE live namespace must override it, because `beam_term::cmp_atoms` falls back to the raw atom **index** for a name the supplied table does not know — which has nothing to do with alphabetical order, i.e. a wrong guard branch with **no error surfaced**.
A name **neither** source knows is a typed error at every such site, never a substituted atom index (an index is valid only against the table it was read with).
`fn atoms<'m>` borrows `*ctx` immutably: take it after the operand reads, never across a `&mut ctx` call.
This is distinct from `atom_name` (the atom-*value* seam for `apply(M,F,A)`/`M:F(...)`), which answers an **owned** `Option<String>` so the caller can hold both across later `&mut` host calls, and reports `BadArg` for a non-atom or unnameable id, never a substituted atom. Gate: `tests/apply_atom_value.rs`.
Pins: `src/interpreter/test.rs::tests::comparisons_order_atoms_by_name_through_the_shared_namespace` (oracle-cross-checked) and the host-side `../../beam_rt/tests/atom_namespace.rs` (the real `sofs:drestriction/2` repro plus a guard over an atom interned after load, over both host routes).

## The BIF suspend channel (host contract for a waiting BIF)
`call_bif_outcome`/`call_bif_in_machine_outcome` are the outcome-aware doors; both return `Result<BifOutcome>` and both **default** to the value wrapped as `BifOutcome::Return`, so a host predating the channel stays byte-identical. A host that answered `true` to `wants_bif_in_machine` is dispatched through the in-machine door only and must override its outcome twin.
`BifOutcome::Suspend` means the BIF did **not** complete and the instruction was **not** applied: no destination register written, no guard branch taken, no frame effect.
The park/resume protocol: the engine reports `RunOutcome::Suspend { func, pc }` **at the BIF instruction itself**; the host pumps its own waiting resource and `Machine::resume_outcome`/`resume_outcome_with_tier` re-enters there, **re-executing the instruction with the same operands**, so the caller sees that call's value and never a value the abandoned attempt computed.
The host must be re-execution safe: a suspending attempt must not consume a one-shot resource.
The engine holds **no** suspended-BIF state — no field, no saved argument vector, not even the BIF's identity — because the park is `(func, pc)` plus the registers, the stack and the FR file, i.e. exactly the frozen `RootSet`, so a moving collector may relocate the operands and the re-execution re-reads them.
**Class A vs class B**: a re-execution-idempotent site (pure operand reads, every write success-only) turns a suspend into the park; the `erlang:apply/3` route, which has already walked its argument list into `x(0..arity)`, instead reports the typed `unsupported("bif suspend at a non-idempotent dispatch site: apply/3 rewrites x(0..arity)")`, because re-executing it would read clobbered registers and apply a different call.
Every tier reports and resumes a suspend: the interpreter folds `Step::Suspend` into `RunOutcome::Suspend`, the native baseline tier carries it out as the `SUSPEND` (`-5`) block-ABI word with its address in the `JitRuntime::suspend` side channel, and `Tier::Cranelift` re-enters through the shared no-reset resume seam `jit::baseline::run_native_resume_outcome`. No tier change on a resume.
A **non-blocking** entry folds a suspend into the typed `error::suspended()` — never `error::blocked()`, never a substituted value.
Tracing deviation: a `call_ext*` arm emits its `call` event **before** the dispatch, so a suspend on a traced run re-emits that event when the instruction is re-executed.
Gate: `tests/bif_suspend.rs`.

## The closure-apply suspend channel (host contract)
`apply_fun_outcome` is the outcome-aware twin beside the value-returning `apply_fun`, returning `Result<FunOutcome>` (`Return(value)`/`Suspend`) and **defaulting** to the value wrapped as `FunOutcome::Return`; an unimplemented `apply_fun` still answers `unsupported("apply_fun")`.
It exists because a host that **interprets** the closure itself (a tree-walking `-eval` evaluator) cannot park its caller through a value-returning door.
`FunOutcome::Suspend` means the application did **not** complete and the instruction was **not** applied; the engine parks on the `call_fun`/`call_fun2` instruction and the resume **re-consults this door with the same `fun` and `args`**, so the host must **continue** the pending application and never restart it (an evaluator body is not re-execution idempotent — the one respect in which this contract is stricter than `BifOutcome`'s).
Gate: `tests/fun_suspend.rs`.

## Per-module unit-test coverage
Every `src/**` module carries `#[cfg(test)]` unit tests beside the code they cover; the per-test names are `grep -n '#\[test\]'` in the owning file, and each module's `//!` docs state what its suite pins.
`context` — hook dispatch, the default-hook `unsupported`, `alloc_words` pointer stability/aliasing, `apply`/`apply_fun` forwarding and defaults, plus the arena census/mark/region-walk suite over `ContextHeap::with_geometry`'s exact growth arithmetic.
`context::reclaim` — the two tiers' 8 exact-number tests over 4-/8-word reservations, including the only place a forward/replaced-region/foreign mark is fabricated (an epoch-matching empty mark is the arena-reset door).
`machine::root_tests` — the `for_each_root` census pins (every rooted file, the five loose terms, the immediate filter, agreement with the `with_roots` word set) beside the four `for_each_root_mut` parity pins (same multiset, same slots relocated, writes observed, `args_scratch` excluded and the parking re-seated around a visitor's `Err`).
`machine` + its split files — the cross-cutting raise funnel over real run entries, the raise-time snapshot on both the caught and uncaught path, registers, frames (push/pop/return, `trim`, Y-slot scoping, `ensure_heap`'s root forwarding, `resumption_addresses`), catch/unwind/landing, run entries, the per-instruction reduction charge, and the position accessors.
`terms` — every constructor↔reader round-trip, `make_map` across the 32-key flat/hashmap boundary, raw-word layout proofs, both map readers over hand-built nodes, and the error cases (flat map >32, fun field >255, binary >64, wrong-type readers).
`terms::alloc_tests` — the rooted-allocation suite: a counting context proves the `*_rooted` builders fire the rooted hook only, a **moving** collector proves the reserve → read → write ordering, and a peak-word table contrasts the arena with the collecting heap.
`operand`, `predicates`, `interpreter`, `families::*`, `jit*`, `exec_census` — one suite per module: operand kinds incl. out-of-range and a missing literal; the label map and the `NO_FAIL` `function_clause` raises; the per-family bodies; the tier roster invariants, the block-ABI target encoding, `JitRuntime` field offsets, the emitter proven by *executing* what it emits, and the cache's namespace partition; the census's 8 tests (off by default, keying by `TestKind`, `first_error`, the TSV shape and sanitisation, and that a raising instruction counts as executed **and** errored).
The integration suites are the sibling node `../tests/`; enumeration: `../TEST_NOTES.md`.

## Long files (role-justified, beyond the ~1000-line guideline)
`context.rs` (3 585 lines) is the entire runtime seam — both traits, `RootSet` and the `MinimalContext` region heap — one cohesive unit whose size follows from that single responsibility, not from unrelated code accumulating.
`exec_census.rs` (794) and `operand.rs` (465) are the largest top-level files after it; every other `src/*.rs` is under 250.
Files inside a subtree with its own `CONTEXT.md` are documented by that node and not listed here.

## Known Issues
- The arena reclaim tiers are host-invoked and nothing in the engine calls them, so an embedding host that never reclaims grows monotonically; `MinimalContext` is a test/embedding heap, not a collector (`context/CONTEXT.md`).
- Reading a term after its `MinimalContext` dropped is a use-after-free that surfaces as a **wrong value** or a term-integrity abort — never a typed error (see the lifetime contract above).
- `ExecutionContext::heap_available` has no call site: no heap-admission memoisation exists, so every `test_heap` crosses the host door unconditionally.
- A host hook reached through `jit_ext_call`/`jit_local_call`/`jit_bif_value`/`jit_tail_call` sees a stale (block-entry-or-earlier) execution position.
- The census under-reports on the native tiers (inlined instructions are invisible) and counts a caught raise as executed-only.
- Opcode/operand coverage gaps, structured-raise degradations and the handoff list are the crate node's: `../INSTRUCTION_SURFACE.md` (`## Known Issues`, `## Handoffs`).

## Notes for Agents
- Read `../CONTEXT.md` (frozen API, execution model, handoffs) before changing anything here; the child `CONTEXT.md` that owns a rule wins over this file.
- This file documents **this node's** files; do not re-add subsystem detail that `machine/`, `interpreter/`, `families/`, `jit/`, `terms/`, `context/` or `bench/` already own.
- Build/test: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --no-fail-fast`; never `cd`, never touch `../../tools/` or `../../../../erts/`.
- Never hand-edit generated artifacts; `ops.tab`-style tables and the `genop.tab` opcode roster are source data.
- Files target ~1000 lines; split by responsibility before growing past it, and record a role justification at the top of a legitimately long file.

## See Also
- `../CONTEXT.md` → the crate-level frozen API surface, execution model, constraints and handoffs.
- `../INSTRUCTION_SURFACE.md` → per-opcode/per-family semantics, design decisions and the full known-gap/handoff lists.
- `../MEASURED.md`, `../PERF.md`, `../TEST_NOTES.md` → measured figures, tier performance snapshots, the test/fixture enumeration.
- `../../../CONTEXT.md` (§2 frozen term layout, §3 error strategy, §4 the engine IR seam and root-set contract) → the workspace contracts this node implements; `../../CONTEXT.md` → the crate-layer node.
