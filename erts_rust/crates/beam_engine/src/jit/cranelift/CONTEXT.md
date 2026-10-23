# `beam_engine` optional Cranelift hot tier (`src/jit/cranelift/`)

## Intent
The optional **hot tier** behind the default-OFF `jit-cranelift` Cargo feature: one Cranelift
function per IR `Function` over the **same** block ABI and the same shared interpreter-backed helper
as the baseline tier. Measurement-gated and off the critical path (DeepDive §F); correctness-first.
With the feature off the crate carries **no** Cranelift dependency and `Tier::Cranelift` evaluates to
a typed `unsupported("jit-cranelift tier not built")` — never a panic, never a silent fallback.

## API Surface
- `mod.rs` — `CraneliftJit` (the `JitBackend`, whose `run_export` override routes through the store),
  `CompiledFunctions` (owns the `JITModule`; accessors
  `entry`/`entry_pc`/`function_count`/`labels`/`highest_native_register`/`is_enterable` + the `inlined_*`
  introspection; `highest_native_register` is both the register-file bound guard's figure and the
  register-write extent `enter` declares — see below), `run_outcome`/`run_label_outcome`/
  `run_resume_outcome`, `run_cranelift*`.
- `cache.rs` — the **retained-artifact store**: `handle(module) -> Result<Rc<CompiledFunctions>>`
  (compile once per sound identity, reuse afterwards) plus the test-gated `len`/`compile_count`/`clear`
  introspection.
- `codegen.rs` — `compile_module` (`emit_pc` per instruction; the module-wide label map it builds is kept
  on the artifact, not dropped); `inline.rs`/`compare.rs`/`shape.rs` —
  the inline-set lowering and shape tests; `report.rs` — `CodegenReport`
  (`inlined_tests`/`inlined_gc_bifs`/`inlined_comparisons`/`inlined_select_vals`/`inlined_tuple_reads`/
  `inlined_list_reads`/`inlined_swaps`/`inlined_tail_calls`).

## Signature and native entry points
Each compiled function has the signature `extern "C" fn(rt: i64, pc: i64) -> i64` (the Rust-side
view is `codegen::NativeFunc`); `rt` in `rdi`, the entry `pc` in `rsi`, the block-ABI word in `rax`.
The `pc` argument is an **entry `pc`**: the function's prologue
(`codegen::emit_pc_entry_dispatch`, emitted by `emit_function`) dispatches on it over the function's
candidate set, checking the function's own `entry_pc` **first** as a one-comparison fast path and
falling back (defensively, unreachable) to `block_for(entry_pc)`. That set is
`codegen::candidate_pcs` — every IR block start plus every non-tail-call continuation
(`call_site + 1`) — restricted to addresses with a real instruction and exposed through
`CompiledFunctions::is_enterable(func, pc)`. The `NativeCode::enter` impl (`CompiledEntries`) gates
native entry on `is_enterable` and calls `code(rt as i64, pc as i64)`, so the shared driver can enter
a compiled function natively at **any** of those addresses — which is how a cross-function non-tail
call's return (`call_site + 1`) is picked up natively instead of on the driver's per-instruction
interpreter step. An address outside the set (a `wait`/`wait_timeout` suspension `pc`) stays on the
interpreter step — never a wrong result. `CompiledFunctions::entry_pc(func)` (the function's entry
`pc`) is kept for introspection; native entry is gated on the wider `is_enterable` set.
`emit_pc_entry_dispatch` is separate from `emit_dispatch_chain` (the helper-*word* continuation
chain): the two share the candidate set but neither shares the other's mechanism, so the helper-word
chain is unchanged. Since the tier caches **no** x-registers across instructions (it addresses
`machine.x` directly), entering a function at a mid-function `pc` with the machine's current state is
correct by construction.

### The register-write extent `NativeCode::enter` declares (root-set soundness)
Inlined code stores **straight into `machine.x`** (`[x_regs + x*8]`: `codegen.rs`'s `XREGS_DISP`/
`x_offset`, `inline.rs`'s `move`/`swap`, the `test`/`select_val`/comparison fast paths), which no
`Machine::set_x` high-water mark can observe. So `CompiledEntries::enter` declares the artifact's own
write extent on the machine **before** the native call, on the path that actually enters native code
(after the `is_enterable` and register-file-bound checks and the `entry` resolution):
`machine.note_x_dirty(highest_native_register + 1)`, or the wide `machine.mark_regs_escaped()` for an
artifact that inlined no register-addressed instruction at all (`highest_native_register == None`).
`CompiledFunctions::highest_native_register` therefore has **two** load-bearing uses in `enter`: the
register-file bound guard (the inlined addressing is unchecked) and this write extent — it is the
*highest x index* the generated code addresses, so the extent is one past it.
Both consequences are contracts, not conveniences:
- `Machine::reset`'s refill is bounded by `used_x_extent() = max(x_mark, x_dirty)`, so without the note a
  deep native write survives the reset and a later run hands a host a **stale word as a live root**;
- a helper the compiled body calls **mid-body** (a deopted `test_heap` →
  `Machine::ensure_heap` → `with_roots`) derives its x-register root prefix from the same figure, so
  without the note a collecting host would be handed a zero-length slice and would miss the relocation
  of every natively written register — a stale pointer after a moving collection.
It is noted **once per entry, before the call, never widened afterwards** (an after-the-fact note would
undo the narrowing the figure exists for), exactly as the baseline tier does it in
`baseline::enter_compiled_cached`. The FR file needs no note: nothing in this tier's lowering stores
into it, so an FR slot is covered by the `set_fr` mark.

`run_resume_outcome` resumes the parked process on this tier. The artifact survives the park (the store
below keeps it mapped, and it carries the module-wide label map, `CompiledFunctions::labels`), and the
tier hands it to the **shared no-reset resume seam**
`crate::jit::baseline::run_native_resume_outcome(machine, module, func, pc, ctx, code)`, which is
`NativeCode`-generic: it zeroes the slice's reduction counter (`Machine::reset_reductions`), re-binds the
run entry (`Machine::enter_run`), takes the artifact's stored label map (`RunLabels::Stored`) instead of
rebuilding one, and drives the same `baseline::drive` loop from the parked `(func, pc)` that every native
tier runs — so there is no second driver loop, no compile-on-demand, no recompile and no cross-tier
fallback. The `Rc<CompiledFunctions>` handle is held for the whole slice (`cache::handle`), which is what
keeps the executable mapping alive under the resumed native code.
The address is entered natively when `CompiledFunctions::is_enterable(func, pc)` holds (one native entry
per function dispatches over `candidate_pcs`); every other address takes the driver's per-instruction
interpreter step, which is the documented deopt, not an error — a mid-block `pc`, and a cross-module
callee (whose blocks this tier holds none of, `NativeCode::module_blocks` = `None`).
A `receive` park reports the `wait` instruction's **branch target** (the `loop_rec`), a reduction
refusal parks at a block entry, and a suspended BIF parks on the BIF instruction itself — which may be
interior, i.e. the per-instruction arm. The park address and the result are both the interpreter's,
clause for clause.

## Inline set relative to the baseline
**A far narrower subset than the baseline's — and nothing here raises.** Every `Instr` the `match` in
`codegen.rs::emit_pc` does not claim falls to `emit_helper_dispatch` → `jit_step_one`, i.e. exactly one
interpreter step (`codegen.rs:672-675`), so this tier has **no** Cranelift-specific `unsupported`
besides its own module errors.
Inlined (the complete set, `codegen.rs:417-672`): metadata no-ops (`Label`/`Line`/`IntCodeEnd`/
`ExecutableLine`/`DebugLine`), `Move` (x-register destination and an x-register or constant source only,
`inline.rs:68`), `Swap` (two in-bound x-registers, `inline.rs:115`), `Jump`, `CallOnly`, `Test` (the
word-compare subset, `inline.rs:544`), the small-int `GcBif` `erlang:'+'/2`/`'-'/2`/`'*'/2`
(`inline.rs:231`), the eight comparison `GcBif`/`Bif` (`compare.rs:206`), `SelectVal` (immediate
alternatives only, `shape.rs:97`), `SelectTupleArity`, `GetTupleElement`, `GetList`/`GetHd`/`GetTl`.
Deopted here but inlined by the baseline (each a working per-instruction interpreter step, never an
error): `FuncInfo`, `CallLast`, `Call`, `CallExt`/`CallExtOnly`/`CallExtLast`,
`CallFun`/`CallFun2`/`Apply`/`ApplyLast`, `Return`/`Deallocate`/`DeallocateReturn`,
`Allocate`/`AllocateHeap`/`TestHeap`/`Trim`, `InitYregs`, `SetTupleElement`, the receive family
(`Send`/`LoopRec`/`LoopRecEnd`/`Wait`/`WaitTimeout`/`Timeout`/`RemoveMessage`), the bitwise half of the
small-int `gc_bif` (`band`/`bor`/`bsr`), the named-BIF accessors (`hd`/`element`/`tuple_size`/
`byte_size`/`length`), and the whole family-owned surface (put_list/put_tuple2, records, funs,
try/catch/raise, floats, maps, bitstrings, `recv_marker_*`) — the baseline lowers all of these in-block
(`../baseline/inlineset/CONTEXT.md`).
Where the two inline sets overlap, the shapes are near-identical, with Cranelift marginally more
permissive in two behaviourally-neutral spots (an un-interned `true`/`false` in `is_boolean`; a
non-integer `select_tuple_arity` alternative). Both remain observationally identical to the interpreter.

## `constant_word`: one definition, three consumers
`inline.rs::constant_word` is a **delegation** to `crate::interpreter::constant_word` — the single authority
(`src/interpreter/select.rs`) on which candidate is decidable by a raw-word compare — exactly as the baseline
emitter's `emit/operands.rs::constant_word` is. Nothing here decides immediacy on its own, so all three consumers
accept the same values: the interpreter's `select_val` ladder, the baseline tier's inlined `select_val`/`test`/
`select_tuple_arity`/`SetTupleElement` plans, and this tier's. The crate-local `small_word` beside it is the
small-int-only reader (the same 60-bit range test) for the `test` lowering's `TestConst::Small` sites, which need
that half alone.

## Constraints
`unsafe` only inside the `mod.rs` module subtree: `mod.rs` declares `#![allow(unsafe_code)]` and its
children inherit it, so `codegen.rs`/`cache.rs`/`inline.rs`/`compare.rs`/`shape.rs`/`report.rs` are safe
Rust and the one `unsafe` block in the `#[cfg(test)] mod tests` is test-only. The tier never touches
`Machine` state a block cannot address — anything requiring a frame pop or context switch deopts to the
shared helper.

## Artifact retention (the store in `cache.rs`)
A `CompiledFunctions` owns an executable mapping plus raw pointers into it, so it is neither `Send` nor
`Sync`: the store is a `thread_local!` `RefCell<ArtifactCache>`, exactly like the baseline tier's
`crate::jit::cache`. Three entries now obtain their artifact through `cache::handle(module)?` —
`run_outcome`, `run_label_outcome` and the `JitBackend::run_export` override (which folds the outcome:
`Returned(v) => Ok(v)`, `Wait{..} => Err(error::blocked())`, `Suspend{..} => Err(error::suspended())`)
— so a repeated run of one module compiles once. `run_compiled` is unchanged (caller-owned artifact),
and the handle is released before the driver runs (no `RefCell` borrow is held across execution).
- **Identity is the baseline's two-part witness, from public accessors only**: `module.checksum()` (the
  load-time MD5, `Copy`) plus `module.atom_table.snapshot()`. The namespace witness is load-bearing —
  Cranelift bakes atom **immediates** into the emitted `move`/`test`/`select_val`/comparison fast paths,
  so a different namespace must be a **miss**, never a wrong answer. Two separately loaded copies of one
  `.beam` are two entries (a deliberate extra compile at worst).
- **A module whose `checksum()` is all-zero (the `Module::default()` sentinel of a hand-built/synthetic
  module) is not cached**: `Key::of` returns `None`, so it compiles per entry — always correct, a wasted
  recompile at worst. The baseline cache's content-fingerprint fallback is deliberately **not**
  duplicated here.
- **Re-entrancy is uncached, never a panic**: `handle` uses `try_borrow_mut()`; on `Err` it compiles
  uncached and returns that artifact (mirroring `jit::cache::run_baseline_cached`'s re-entrant arm).
- The test-gated `len`/`compile_count`/`clear` are `pub(super)` only — the in-module suite reaches them;
  nothing is re-exported from `jit/mod.rs`.

## Cross-module transfer
The tier runs on the **shared** driver (`../baseline`), which handles a `Step::Enter` (a
`call_ext*` the host resolved through `ExecutionContext::resolve_external_call`) by switching module
context and re-entering. The Cranelift tier inherits `NativeCode::module_blocks`'s default (`None`),
so the callee is not accelerated — the driver runs it on its per-instruction interpreter step — but
it *executes*, so a compiled caller reaches the callee and returns correctly; the return from a
**same-module** cross-function non-tail call is now picked up natively at its continuation `pc`
(see "Signature and native entry points"). Its own `run_resume_outcome` resumes through the shared
`baseline::run_native_resume_outcome` seam.

## Measured verdict — the tier stays DEFAULT-OFF
The feature is measurement-gated, and the measurement says **no**: Cranelift is a
correctness-complete mechanism but **not** a performance win over the baseline tier.
Every figure is a **range over independent runs**, one timing process at a time, pinned to a single
core (`taskset -c 3`), on a shared 14-CPU sandbox whose `/proc/loadavg` reads **≈9.5–10 at both ends
of every run** (neighbouring worktrees are the load; it swings by an order of magnitude across days,
so only the *ratios* are signal — never an absolute wall time, and never a single sample).
Column ratios are **cost** ratios (`>1` = slower than the reference).

Per-workload ratio of Cranelift to the baseline, in-crate `crate::jit::bench` (`... -p beam_engine
--features jit-cranelift --test jit_bench -- --ignored --nocapture`), from the min `ns/op` of the
3000-run timed loop. That table runs its tiers in a **fixed** order per workload (interpreter →
baseline → Cranelift), so it is position-biased against Cranelift and is quoted only as the shape
picture; the position-balanced evidence is the ABBA table below.

| workload (shape) | CL ÷ baseline |
|---|---|
| `walk/2` — list walk | **≈0.48×** (the only win) |
| `slice:fib/1` — call chain | 0.95× (tie) |
| `aggr/2` — aggregate reads + `'*'` | 0.99× (tie) |
| `call_ext/0` — declined host seam | 1.07× (tie) |
| `map/2` | 1.08× |
| `build/0` | 1.17× |
| `select/1` | 1.24× |
| `cmp/2` — comparison BIFs | **1.25×** |
| `kernel/2` — inner-product loop | **1.36×** |
| `wide/2` — wide tuple/list reads | **1.39×** |
| `cross/2` — cross-module call | **1.45×** |
| `send_recv/2` — two-process mailbox | **2.18×** |
| `sum/1` — non-tail recursion | **2.50×** |

The order-balanced ABBA harness (`bench.rs`, the `#[ignore]`d `cl_bench` test, `--lib -- --ignored
--nocapture cl_bench`) brackets the same picture with per-round paired ratios (2 samples/tier × 15
rounds, so position drift cancels); paired means over three independent runs:

| kernel | paired cranelift/baseline over 3 runs |
|---|---|
| `aggr` | 0.988–1.009× (tie) |
| `arith` (256 chained `'+'`) | **1.616–1.621×** |
| `cmp` | **1.364–1.422×** |

So the tier is a **tie** on the aggregate-read mix, **≈1.6× slower** on chained small-int arithmetic
and **≈1.4× slower** on comparison-BIF streams.

**Codegen cost — what the hot tier must earn.** Cranelift's one-time compile is **≈6.5–60×** the
baseline's (cheapest on `walk/2` at ≈6.5×, dearest on `build/0` at ≈60×; the range widens under heavy
host load, which inflates the slower compiler far more). A hot tier that compiles that much more
expensively must win on steady-state throughput to earn that cost, and outside `walk/2` it does not.

Why: Cranelift re-loads/re-stores the x-register slots per instruction and **deopts
`call_last`/`call_only`** (a strict subset of the baseline's `Instr` set), whereas the baseline is a
hand-written x86-64 backend that caches the five most-touched x-registers per block, addresses the
rest directly in `machine.x`, links the local tail calls natively and reuses blocks across runs
(§F/`../CONTEXT.md`). Its only real win is the list-walk shape (`walk/2`, ≈0.48× the baseline); the
best everything else reaches is a tie (aggregate reads, the call chain, a declined `call_ext`) or a
mild-to-serious loss (maps, tuple/list builds, `select_val`, comparison streams, wide reads,
cross-module, non-tail recursion, the mailbox) — which cannot justify 6.5–60× the compile latency on a
tier whose whole point is being *hot*.

Consequence: `jit-cranelift` **remains default-OFF** — a correctness-complete mechanism and an
honest negative performance result, not a selected tier. Closing the gap is not a bounded change: it
means porting the baseline's register cache, its direct `machine.x` addressing and its tail-call
linking into the Cranelift lowering. Re-test with `bench.rs` before ever revisiting.

**Measurement caveats (state them with any figure).** The in-crate harness uses **synthetic
modules**, so it covers the tier's inline families, `CallOnly` tail calls and a declined `call_ext`,
but it is **not** the `foreign` / `list_walk` / `gc_bound churn` workloads of `tools/bench`:
`beam_rt` has **no `Tier::Cranelift` selection** (`beam_rt/src/runner/tier.rs` maps `BEAM_RT_TIER` →
`JitMode::{Auto,Interpreter,Baseline}` with `Auto | Baseline ⇒ Tier::BaselineJit`), so the Cranelift
tier is not reachable end-to-end and cannot be measured through the runtime. Also note the `foreign`
workload's *interpreter* side carries a known super-linear `beam_rt` pathology owned elsewhere, so it
is not a clean tier comparison even where reachable.

## Known Issues
- **A cross-module callee runs per-instruction here.** `NativeCode::module_blocks` keeps its default
  `None`, so a callee reached through `Step::Enter` (a cross-module `call_ext*`, or a raise landing in
  another context) executes through the per-instruction interpreter step rather than compiled blocks.
  Correct and clause-identical to the interpreter, slower than a compiled callee would be.
- **The store is per thread and unbounded.** A `CompiledFunctions` owns an executable mapping, so the
  store is a `thread_local!`; a run on another thread recompiles (correct, slower), and nothing evicts
  (the tier is default-OFF and `beam_rt` never selects it, so the retained set is bounded in practice;
  `clear` exists for the tests).
- **A synthetic module is cached under its content fingerprint.** The store keys on
  `crate::jit::cache::CacheKey` — the ONE identity scheme both native tiers use — so a `Module` with no
  load-time digest (an all-zero `checksum()`, the `Module::default()` sentinel) is identified by the
  fingerprint over its decoded IR: two runs of one synthetic module compile once, and an edited module is
  a different entry. A fingerprint collision would be a wrong answer, which is why the key mixes the
  module content with the atom-table snapshot, exactly as the baseline cache does.

## Routing Table
- `mod.rs` → the backend (`CraneliftJit`, incl. the `run_export` override), `CompiledFunctions` (the artifact: native entries, `is_enterable`, `labels`), `CompiledEntries`' `NativeCode` impl, `run_outcome`/`run_label_outcome`/`run_resume_outcome`, `run_cranelift*`.
- `cache.rs` → the thread-local retained-artifact store: `handle(module) -> Result<Rc<CompiledFunctions>>` keyed on `crate::jit::cache::CacheKey` (the shared `ModuleKey` — load-time MD5, else a content fingerprint — plus the `AtomSnapshot`), and the test-gated `len`/`compile_count`/`clear`.
- `codegen.rs`/`inline.rs`/`shape.rs`/`compare.rs` → the inline set + shape tests; `report.rs` → the introspection report; `tests.rs` → in-module unit tests; `bench.rs` → the `#[ignore]`d ABBA Cranelift-vs-baseline measurement harness (its synthetic-`Module` builder, BIF hook and ns/op primitives are the shared `crate::jit::bench::support`, not a second copy).
- Tier equivalence on real modules: crate `tests/jit_cranelift.rs`, `tests/jit_cranelift_real.rs` (see `../CONTEXT.md`).
- `bench.rs` is run with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --features jit-cranelift --lib -- --ignored --nocapture cl_bench`.

## Test Strategy
In-module unit tests (`tests.rs`) cover the backend naming/availability, the per-function entry
points, the reported inlined set, a direct native call into a compiled entry, and
`CompiledFunctions::is_enterable` — the latter pinned against an independent test-side oracle (IR
block starts ∪ non-tail-call continuations) on the `coverage.beam` fixture, asserting the entry `pc`
and every `call_site + 1` are enterable while an interior `pc` no boundary names is not. Tier
equivalence on real modules lives in the crate's `tests/jit_cranelift.rs` / `tests/jit_cranelift_real.rs`.
The artifact store's own coverage (5 cases, all in `tests.rs`, each comparing to `Tier::Interpreter`
where it executes something): `a_second_run_of_the_same_module_reuses_the_cached_artifact` (one compile
for two runs of one loaded module, plus the deliberate miss for a separately loaded copy),
`the_artifact_carries_the_module_wide_label_map` (every label resolves as a rebuilt
`build_label_map(module)` does, and `NativeCode::labels` answers `Some` ⇒ `RunLabels::Stored`),
`a_receive_park_reports_the_interpreters_address_and_keeps_the_artifact` (the `recv.beam` `recv:wait/0`
park address equals the interpreter's resumption address — proved by resuming there — and the artifact
is still held afterwards), `a_synthetic_module_is_cached_under_its_content_fingerprint` (all-zero
checksum ⇒ `CacheKey`'s content fingerprint ⇒ two runs compile once, `len() == 1`,
`compile_count() == 1`, and an edited module hashes differently) and
`clear_drops_every_artifact_and_resets_the_compile_count`.
The tier's own `run_resume_outcome` has **five** park/resume cases, each compared clause for clause with
`Tier::Interpreter` (`Machine::resume_outcome` on the interpreter side,
`Machine::resume_outcome_with_tier(Tier::Cranelift)` on the tier side):
`a_reduction_refusal_parks_the_machine_and_the_tier_resumes_it` (a `reduce` that answers a typed `Err`
parks at the block entry with nothing of the block executed; the resume re-charges only the parked
address — total charges = uninterrupted + 1, and `Machine::reductions()` is the resumed slice's own),
`a_receive_park_on_the_tier_resumes_with_the_interpreters_result` (`RunOutcome::Wait { func, pc }` equals
the interpreter's address; the resume with a message arriving completes identically and reuses the
artifact the park compiled), `a_resume_at_the_reported_wait_pc_advances_by_the_interpreters_rule` (the
`wait` park address and its resumed result are the interpreter's),
`a_resume_parked_in_a_non_zero_module_context_agrees_with_the_interpreter` (parked inside a cross-module
callee: the driver lands in that context and runs the callee per-instruction — correct, not an error) and
`a_resume_does_not_double_count_the_reduction_charge` (both tiers restart the slice counter at the
resume, so the parked slice's charges are never counted twice).
The register-write extent has its own **three** cases, one for each half of the contract:
`a_native_entry_declares_the_register_write_extent` (a synthetic `f/0` whose body is one natively inlined
`move` into `x9`; the runtime block is built by hand and the compiled entry called directly, so **no
interpreter step runs in between** — the fresh machine's extent is `0` before the call and exactly
`DEEP + 1` after it, then `Machine::reset` clears the deep slot and `refilled_words() == DEEP + 1` proves
the refill is the declared extent, not the two files),
`a_driver_run_on_a_real_module_declares_the_extent_too` (the same declaration on the driver's
`NativeCode` path over `slice.beam`: the result is unchanged and `used_x_extent() >=
highest_native_register + 1` after the run, with the following reset wiping every x slot) and
`the_declared_extent_lets_a_mid_block_helper_see_native_writes` (the root-set half: a synthetic body that
inlines two `move`s — one into `x9` — and then reaches the deopted `test_heap`, whose helper path
(`ensure_heap_full` → `with_roots`) is recorded by an in-test `RootProbeCtx`; exactly one admission point
is seen, its `live` hint is forwarded unchanged, and the handed x slice covers the deep register with its
moved value).
