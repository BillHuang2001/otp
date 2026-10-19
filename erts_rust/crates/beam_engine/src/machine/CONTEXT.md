# `beam_engine` — Machine state core (`src/machine/`)

## Intent
The process-state core of the Stage-1 engine: the register files, the Erlang stack, the call-frame and
exception catch-frame stacks, the module-context registry, the run entries, and `Machine::drive` — the
module-context-aware **dispatch loop that IS the interpreter's hot loop** (the `run_loop*` entries in
`../interpreter/dispatch.rs` are thin wrappers over it).

## API Surface (crate-internal; methods on `impl Machine`)
- `mod.rs` — `Machine` (x-register file, float-register (FR) file of `Eterm` term slots, Erlang stack,
  call frames, exception catch frames, literal area, module-context registry, the **raise-time snapshot**
  `raise_snapshot: Vec<(u32, u32, u32)>` — the pre-unwind `(context, func, pc)` chain, the engine's
  replacement for C's saved raw `StackTrace` (`c_p->ftrace`) — latched A.8 trace gate, reduction counter,
  the **in-machine BIF-dispatch gate** `bif_in_machine_on` — the cached
  `ExecutionContext::wants_bif_in_machine` answer, latched once per run by `Machine::drive` **and** by the
  native baseline tier's driver, read by `Machine::dispatch_bif_call` — and the fourth latched gate,
  **`census_on`**, the per-run copy of `exec_census::is_enabled()` that the per-instruction funnel at
  `interpreter/dispatch.rs::step_instruction_inner` tests instead of touching the census's thread-local on
  every instruction (cleared by `reset`, latched by both drivers, so a mid-run `enable()` takes effect at the
  run's next entry)); `MAX_REGISTER`/`MAX_FLOAT_REGISTER`; `reset`; the **root-set hand-off**
  (`stack_mut`, `with_roots` handing over the live `RootSet`, and `alloc_rooted` — the convenience
  wrapper that calls `ExecutionContext::alloc_words_rooted`); and the **off-heap refc-binary handle view**
  `refc_binaries()` — the read-only slice of the `RefcBinary` payload handles `alloc_refc_binary` built,
  which a host clones into a copy's **source** off-heap table so the copy engine can resolve a refc
  binary's payload by data pointer (a cloned handle's refcount keeps the payload alive past the machine);
  with it the **retention seam** `off_heap_overhead_bytes()` (the reservoir's backing-store bytes) and
  `retain_refc_binaries(keep)` (host-supplied reachability pruning, returning the number of handles
  released) — see `## The off-heap refc-binary retention seam` and the `offheap.rs` bullet.
  Also on `mod.rs`: **`Machine::register_count()`** — *this* machine's X-register file size
  (`x[0..register_count())`, i.e. the bound `Machine::x` indexes into), which is what a caller that walks
  the argument registers must stop at when the file was narrowed with `Machine::with_max_register`;
  reporting the machine's own size rather than the `MAX_REGISTER` constant is what keeps such a walk inside
  the slots that exist. And the class-atom state: `class_atoms: [Eterm; 3]` (indexed by
  `exception_class_index`, `THE_NON_VALUE` = unresolved) plus the **per-run `class_atoms_live: bool` latch**
  described under `catch.rs`.
- `registers.rs` — x/FR access (`get_fr`/`set_fr`, bounds-checked → `bad_register`; `x_mut`/`fr_mut` as
  the single-view root-set accessors); `fr_mark()` — the **live leading FR-slot count** (the FR high-water
  mark, `0` ⇒ no live FR slot), the FR twin of the `x[0..live)` root-set bound, so a host can publish/scan
  only the slots an FR write actually touched. The refill bound is the **used extent** of each file:
  `used_extent(len, mark, dirty)` = `max(mark, dirty)` clamped to the file length, where the mark is raised
  by `set_x`/`set_fr` and the *dirty* extent by every writer that cannot report slots — `x_mut`/`fr_mut`
  (the whole file), `with_roots` (the prefix it hands over) and the native driver (the artifact's own
  `CompiledModule::native_write_extent`, noted **before** the block runs). Every slot at or beyond the extent
  is `NIL` by the refill invariant, which is what makes the narrow root hand-off sound by construction
  rather than by host promise. `Machine::reset` clears the raise snapshot  alongside the module contexts (both belong to the run that is starting) **and clears all four raise
  latches** (`pending_function_clause_args`/`raise_function_clause_args`/`pending_supplied_trace`/
  `raise_supplied_trace`).
- `mod.rs` — the four raise latches. The failed-clause pair:
  `pending_function_clause_args: Option<Eterm>` (one-shot — armed by the raise site, consumed by the
  `throw_with_trace` that lands it, so a propelling `Err` cannot leave a stale list for a later trace) and
  `raise_function_clause_args: Option<Eterm>` (the **live raise** state, set on every `throw_with_trace` and
  consumed exactly once by the two cooked-trace builders). The explicitly-supplied-trace pair:
  `pending_supplied_trace: Option<StackTrace>` (one-shot, armed only when an incoming `BeamError::Raise`
  carries **non-empty** symbolic frames — the `erlang:raise/3` trace-list shape) and
  `raise_supplied_trace: Option<StackTrace>` (the live state for the raise being landed).
  **Rooting: `Machine::with_roots` (the single root hand-off behind `alloc_rooted`/`ensure_heap`) PARKS the
  two armed heap-term latches** — `raise_function_clause_args` then `pending_function_clause_args` — onto the
  tail of `self.stack` for the duration of the hand-off, hands over the `RootSet`, then **re-seats** each
  field from its slot (`self.stack.get(cursor).copied()` — the stack is rooted in full, so a conforming host
  relocates the parked word and the engine reads the *moved* term back), and finally `stack.truncate(base)`.
  With both latches `None` the `RootSet` is handed over **unmodified** (byte-identical fast path). The
  supplied-trace pair is plain `Vec<StackFrame>` **metadata**, not a heap term, so it needs no rooting.
  `Machine::pending_exception` is **not** parked here — `beam_rt` roots it via `Registers::extra_roots`.
- `frames.rs` — `allocate*`/`deallocate*`/`trim`, the typed `Continuation` CP slot, `Machine::ensure_heap`
  (the `ensure_heap_full` root-set admission point that hands over the whole `RootSet`), and the
  caller-chain introspection (`resumption_contexts`/`resumption_addresses`/`raise_snapshot`).
- `catch.rs` — the `CatchFrame` stack + the raise/unwind entry (`throw`/`throw_error`/`throw_with_trace`)
  and the **uniform raise funnel**: `throw_with_trace` first REBUILDS `raise_snapshot` (the raising frame
  from `current_code`/`current_context`, then every live caller from `resumption_contexts`) — **before** any
  unwind and before the early no-live-frame `Err(BeamError::Raise)` return, so the pre-unwind chain
  survives for the handler. It also promotes the failed-clause argument list: it takes the one-shot
  `Machine::pending_function_clause_args` and stores it as the live `Machine::raise_function_clause_args`
  (a raise that carries no such list clears it, so no later trace can inherit a stale one); it does the same
  for `Machine::pending_supplied_trace` -> `Machine::raise_supplied_trace`.
  **The exception-class atoms and `namespace_atom`.** `Machine::ensure_class_atoms(ctx, module)` resolves every
  still-unresolved `error`/`exit`/`throw` atom from the engine's live atom namespace and installs it with
  `Machine::set_class_atom`; an atom already installed is never overwritten, and the per-run **`class_atoms_live`
  latch** makes the whole body one `bool` test in steady state (it is cleared by `Machine::reset`, because the class
  atoms belong to the run that is starting). `Machine::class_atom` then reports the typed
  `unsupported("exceptions: no atom for the '<class>' exception class …")` for a class neither source can name —
  never a substituted atom. The name resolution itself is the free function `namespace_atom(ctx, module, name)`:
  `ctx.atoms(module)` (the host's live namespace seam) first, the running module's load-time snapshot as the
  fallback, `None` when neither knows the name. C reads its `am_error`/`am_exit`/`am_throw` **globals**, and the
  seam is the engine's equivalent: `beam_rt` answers with its ONE live shared table, while a host that does not
  override `atoms` keeps the module snapshot (byte-identical to the pre-seam behaviour). The `atoms` borrow is
  immutable and ends at the `Copy` `Atom` it yields, so a caller may make its next `&mut ctx` call right after.
  `raise_stacktrace(entry_module: Option<&Module>, func, pc, first_frame_args: Option<Eterm>) -> StackTrace`
  derives the symbolic `StackTrace` (raising function, then every live caller innermost first), giving
  `first_frame_args` to the **raising** frame only; `Machine::with_uncaught_frames(&mut self, err, entry_module, func, pc)`
  attaches it to an uncaught `BeamError::Raise` (leaving a non-raise error, an already-framed raise and an
  **engine diagnostic** — a reason atom prefixed `unsupported_opcode: `, the `error::unsupported` sentinel —
  untouched). The funnel's first half runs at the `step_instruction` boundary (`../interpreter/dispatch.rs`),
  its **second half at the `drive` boundary** for a raise that surfaced inside a cross-module callee (`drive.rs`).
  The live latch splits around `symbolic_stacktrace`'s one rooted reservation: the **read** half
  `Machine::live_raise_function_clause_args()` yields the list *before* it (the sizing needs only the
  list's move-invariant **length**) and the **consuming** half `Machine::take_raise_function_clause_args()`
  (called **exactly once** by each of the two cooked-trace builders) consumes it *after* it — the
  reservation may have moved the list, and `with_roots` re-seats the field
  from the slot the host wrote the moved word back through — handing the re-seated list to the first
  frame's `A` slot (every outer frame keeps its declared arity); the list's elements are carved **inside**
  the reservation, so no element word is held across a collection. `Machine::take_raise_supplied_trace()` is the
  consuming half of the supplied-trace latch: `symbolic_stacktrace` renders those frames **instead of** the
  snapshot heuristic when it is armed, and an absent/empty supplied trace leaves every path byte-identical.
- `context.rs` — the `ModuleContext`/`Contexts` registry; `current_context`/`register_context`/`switch_context`/
  `module_context`.
- `offheap.rs` — the off-heap refc-binary **retention seam**: `Machine::off_heap_overhead_bytes()` (the sum of
  `RefcBinary::reserved_bytes()` over `refc_binaries`) and
  `Machine::retain_refc_binaries(keep: FnMut(*const u8) -> bool) -> usize` (drops the handles the predicate
  rejects, returning how many were released, and prunes `writable_payloads` in lockstep). It is a separate
  file to keep both modules under the line budget; a child module of `machine` sees the parent's fields.
  Detail: `## The off-heap refc-binary retention seam`.
- `local_callee.rs` — the **local-callee identity memo** `Machine::local_callees` (`LocalCallees`,
  `CalleeKey`, `CalleeTarget`): the `(module name, function name, arity)` derivation the local-call seam
  (`../interpreter/dispatch.rs::local_call_seam`) would otherwise repeat on every local
  `call`/`call_only`/`call_last`, memoized per `(module address, function index)` in a fixed
  `CAP = 8` array held as two parallel slices — a compact `keys: [CalleeKey; 8]` (16 bytes per slot,
  the only thing the hot scan reads) beside the `targets` payloads that own their name text.
  **Policy: insert-at-front on a miss; a hit never reorders** (the entry keeps its slot), and the
  oldest *insertion* falls off the end. That is deliberate: `list_walk`'s inner loop alternates two
  callees (`sum/1`, `walk/3`), so an MRU move-to-front fired on every call and memmoved up to eight
  owned entries in the innermost loop. With the fixed policy the seam's share of **retired
  instructions** is **1.85 %** (interpreter) / **4.05 %** (baseline tier) against the un-memoized
  derivation's 3.22 % / 7.93 %, i.e. **−0.5 % / −2.2 %** retired instructions per walked element
  (model-free `perf stat -e cpu_core/instructions/` route, three reps agreeing to 0.01 %). An
  un-nameable callee is memoized as `None` and is then consulted **zero** times. **Invariant: the
  memo holds the identity derivation only — never the host's verdict**
  (`ExecutionContext::resolve_local_call` is consulted, and spends one reduction unit, exactly once
  per local call). Cleared by `reset` (`registers.rs`), which is what keeps the address key ABA-free;
  a resume does not clear it.
- `location.rs` — execution-position introspection (`current_code`, `current_function`, `current_location`,
  `initial_call`, `current_stacktrace`, `current_module`/`active_context`) **and** the parked-`receive`
  deadline seam `Machine::wait_deadline`/`timeout_delay` + `WaitDeadline { delay_ms, resume }`: it reads the
  `wait_timeout` at `current_code` (the blocked instruction, exact because a blocking op is never JIT-inlined)
  in the module `active_context()` names — the callee's own module when the park is inside a cross-module
  callee (context != 0), else the passed entry module — and reports the `timeout` continuation (`pc + 1`) a
  host re-enters when its own timer fires. `Machine::timeout_delay` accepts every operand shape the compiled
  `wait_timeout` can carry: an `Integer`/`Literal` delay, an `X`/`Y` register, and an `Operand::Typed { reg, .. }`
  (the `{tr,..}` typed-register operand, unwrapped to its `X`/`Y` register — the type annotation is a
  compiler hint, not a heap type). The engine arms no timer: a non-zero `wait_timeout` just suspends.
- `run.rs` — `run_export`/`run_label` (non-blocking) and `run_export_outcome`/`run_label_outcome`/`resume_outcome`
  (blocking-aware); the **tier-selecting** entries `run_with_tier`/`run_export_outcome_with_tier`/
  `run_label_outcome_with_tier` (first entry) and `resume_outcome_with_tier` (**resume** — a slice that suspended on a
  native tier resumes on it: `Interpreter` → `resume_outcome`, `BaselineJit` → the cached no-reset baseline resume,
  `Cranelift` → a typed error); `RunOutcome` (`Returned` / `Wait`), re-exported at the crate root.
- `drive.rs` — **`Machine::drive`**: owns the `(func, pc)` position *and* the module context those indices belong
  to, folds every `Step` back into that position, and charges **exactly one reduction per interpreted instruction
  before the instruction's effect** through the defaulted `ExecutionContext::reduce` (the pre-emption point a
  non-allocating loop reaches on this tier — the native baseline tier's driver loop charges the same hook once per
  block entry instead, see `../jit/baseline/CONTEXT.md`; read back through `Machine::reductions()`/`reset_reductions()`). The charge precedes
  the body (C's `DISPATCH` order), so an `Err` from the hook pre-empts the instruction **without applying it**: the
  machine is left parked at that `(func, pc)` — recorded by `Machine::set_current_code` before the charge — and a
  `Machine::resume_outcome` there applies it exactly once. It also latches the **in-machine BIF-dispatch gate**
  (`bif_in_machine_on`, the cached `ExecutionContext::wants_bif_in_machine` answer) so every BIF dispatch in the run
  goes through the single `Machine::dispatch_bif_call` funnel, and threads the run's **entry module** (context `0`)
  down to it via `step_instruction_with_entry` — the same latch the native baseline tier's driver performs. It then re-binds the
  literal area exactly on a `Step::Enter` into a different context. It also runs the raise funnel's second half
  (`Machine::with_uncaught_frames`) over a raise that surfaced inside a cross-module callee — the driver is where
  the run's true entry module is known, so those caller frames are named against the right module.
- `tests.rs` — the cross-cutting end-to-end tests of the raise funnel: one synthetic module per raise kind
  (`throw`/`error`/`exit` `raw_raise`, a BIF error, `func_info`, a nested call, a cross-module callee), each run
  through a real run entry and asserting the propagated `Err(BeamError::Raise)` frames name the raising function and
  every live caller innermost first, plus the guard that a caught raise lands unchanged. It also pins the
  local-call seam's memo against a counting host: two consecutive local calls consult `resolve_local_call`
  **twice** while the identity is derived **once**, the memo holds the callee's own names, a served `call_last`
  pops its frame like the in-machine route, and `reset` clears the memo. `test_support.rs`
  (`cfg(test)` only) is the synthetic-module builder (`Fixture`) and cross-module seam context (`seam`) these share
  with `drive.rs`'s tests.
- `deadline_tests.rs` — the parked-`receive` deadline driven to completion: a **nested** run through the
  non-blocking `run_export` (whose fold to `blocked()` must leave the machine parked, so `wait_deadline` still
  reports the deadline) and a blocking `run_export_outcome` park, each resumed at the deadline's `timeout`
  continuation to run the `after` clause — on a real module compiled by the system `erlc` and cross-checked
  against `erl` (loud skip when `erlc`/`erl` are absent); plus a cross-module-callee park proving the deadline
  is read from the callee context, never the entry module.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no panic, no `unwrap()` on
  runtime data; an unsupported opcode/operand is the documented `error::unsupported`.
- `unsafe` only where the crate `CONTEXT.md` marks it, each block behind a `// SAFETY:` rationale.

## Notes for Agents
- **`Step::Enter` is the only way a run changes module context.** All three producers switch the context at the
  driver: a cross-module `call_ext*`, a cross-module `return` onto a `Continuation::Address` in another context, and
  a raise landing in another context's live catch frame (pinned by `drive.rs`'s tests
  `a_cross_module_return_switches_the_machine_back_into_the_caller_context` and
  `a_raise_landing_in_a_callers_catch_frame_switches_into_the_callers_context`).
- `reset` (`registers.rs`) clears the register files, stacks, catch frames, contexts, the local-callee identity
  memo (`local_callees` — its entries are address-keyed, so they belong to the run that filled them; that is also
  what makes the key ABA-free), the raise snapshot, the pending
  exception, the exception-class atoms + their `class_atoms_live` latch, the execution-position fields and the
  trace/reduce gates, but **not** the `refc_binaries` arena, the `writable_payloads` registry, or
  `bif_in_machine_on` (latched per run into the machine by both drivers; only `Machine::new`,
  `machine/mod.rs`, ever sets it); a resume (`resume_outcome`) resets nothing, so the contexts and the raise snapshot
  both survive it. The two off-heap rosters are released only by a host call to
  `Machine::retain_refc_binaries`.

## See Also
- `../interpreter/CONTEXT.md` → the per-instruction dispatcher (`step_instruction_with_entry`, also the JIT deopt
  target).
- `../CONTEXT.md` → the engine's binding execution semantics; `../jit/CONTEXT.md` → the native tiers' block ABI.

## `Machine::writable_payloads` — the writable-append registry

A small side list of `(payload base, capacity in bytes)` for the off-heap payloads this machine
allocated **for appending** (`bs_init_writable`'s backing store, C's `BIN_FLAG_WRITABLE` binary),
deposited by `Machine::mark_writable(base, capacity)` and queried by
`Machine::writable_capacity(base) -> Option<usize>` (a linear scan, newest entry preferred; no
BEAM-heap allocation). The capacity recorded is the payload's **real** one, read back from its control
header (`RefcBinary::with_capacity` via `Machine::alloc_refc_binary_with_capacity`, a 4x
`DEFAULT_RESERVATION_FACTOR` reservation), never the `bs_init_writable` hint. The frozen term layout
carries no capacity word, so the capacity has to live beside the machine; keying it per machine also
gives C's `erts_pin_writable_binary` property for free — a payload another process's copy points at is
not in *this* list, so it is never written in place here.

Only `mark_writable` entries are writable: an ordinary binary's payload length is never taken as a
capacity (the roster `Machine::refc_binaries` is *not* consulted). The list is **never cleared by
`Machine::reset`**, for the same reason `refc_binaries` is not — a registry entry must never outlive
the payload it names — and it is pruned only by `Machine::retain_refc_binaries`, which drops every
entry whose base no longer names a live retained payload **in the same call** that releases the
handle, so a lookup can never answer with a freed block's address or a stale capacity.
`Machine::writable_payload_count()` reports how many registrations are live.

**Growth keeps the two books straight.** `Machine::grow_writable_payload(base, wanted_bytes)` is the
one widening door (`beam_alloc::RefcBinary::ensure_capacity`; C's `GROW_PROC_BIN_SIZE` schedule) and
it re-keys (`rekey_writable`) before returning: an `InPlace` outcome moves only the cached capacity, a
`Moved` outcome moves the key to `PayloadRelocation::to` so a stale key can never answer for the freed
block, and both then restore the `len == capacity` handle invariant through
`Machine::cover_writable_capacity` (C's `bin_size`; a relocation copies exactly `len()` bytes, so this
is what keeps the appender's bytes alive). Every refusal — an unregistered payload, a shared handle, the
`max_capacity` cap — folds into `Ok(None)` (the caller promotes to the copy-rebuild form), because each
is a runtime condition with a correct fallback, not an internal fault. Republishing the accumulator's
own `[BinRef][ErlSubBits]` pair over a moved base is the *caller's* job (`raw::InPlace::grow`): this
door is safe Rust and never dereferences a term. It is consulted **only on a shortfall**, since it
scans the roster; `Machine::retain_refc_binaries` is not on the append path.

**A payload another party shares can no longer grow.** `beam_alloc`'s mutators refuse unless
`refcount() == 1`, so a shared payload is never reallocated behind another handle's back.
`beam_rt`'s `crates/beam_rt/src/runtime.rs::adopt_escaped_binaries` clones every handle of a finished
machine into its escaped-payload store, so a writable accumulator that **survives a run boundary**
has `refcount() >= 2` and any later append needing growth falls back to the copy-rebuild form
(byte-correct, amortisation lost). An accumulator whose whole loop stays inside one run keeps growing.

Consumer: `families::bitstrings::raw::{writable_append_target, InPlace, payload_aliased_elsewhere}`.

## The off-heap refc-binary retention seam (`offheap.rs`)

`Machine::refc_binaries` is the machine's off-heap payload reservoir; it keeps every refc binary the
machine built alive and address-stable, and `Machine::reset` never clears it (a returned term may still
name a payload after the run that built it). Reclamation is therefore a **host** decision, taken after a
reachability sweep over the process roots, and the seam is two infallible, allocation-free, panic-free
`pub` methods:

- `Machine::off_heap_overhead_bytes() -> usize` — the reservoir's **backing-store** bytes: the sum of
  `beam_alloc::RefcBinary::reserved_bytes()` over the handles. The reservation (not `len()`) is what a
  host charges for, matching `beam_gc::sweep_unreachable_binaries`/`beam_gc::BinVheap` accounting: an
  exactly-sized payload has `len() == capacity() == reserved_bytes()`, while a `bs_init_writable`
  accumulator holds a 4x reservation whose spare room is real memory. The consumer is words-based, so the
  host converts with `bytes / size_of::<beam_term::Eterm>()` (8, a truncating division) and compares
  against `beam_gc::BinVheap::triggered`'s floor, the policy's `min_bin_vheap_words`.
- `Machine::retain_refc_binaries(keep: FnMut(*const u8) -> bool) -> usize` — drops every handle whose
  payload data pointer the predicate rejects and returns the number of handles **released** (a released
  handle frees bytes only when it was the last one, `refcount() == 1`). Retained payloads keep their exact
  `data_ptr`, so compaction never invalidates a live `BinRef.val` (the frozen `beam_term::refc` contract)
  and never moves a block; payloads are always found by `data_ptr` search
  (`grow_writable_payload`/`cover_writable_capacity`), so the compaction is invisible to those doors. The
  same call prunes `writable_payloads` in lockstep (above).

**The predicate is the caller's soundness assertion.** The engine cannot verify it: the reservoir holds
handles, not roots. A false reject frees a payload a live term still names (a dangling `BinRef.val`); a
false accept is merely a leak. The intended use is `beam_gc::for_each_reachable_binary_payload`'s
reachable-pointer set as the predicate, after first **cloning** the handles the host keeps beyond the
machine into its own off-heap table (the clone bumps the refcount, so the payload survives the drop).
Pinned by `offheap.rs`'s unit tests (address stability across a retain, released-reservation accounting, a
shared payload outliving a released handle, writable-registry pruning both ways, the empty no-op).

## The local-call seam's memo boundary (`Machine::local_call_seam`)

All five call sites — the interpreter's three local-call arms (`../interpreter/dispatch.rs:421`/`:467`/`:496`) and the baseline tier's two helpers (`../jit/baseline/call.rs:398`, `../jit/baseline/runtime.rs:1172`) — route through `Machine::local_call_seam` (`../interpreter/dispatch.rs:1109-1142`), which consults the host's `ExecutionContext::resolve_local_call` (`../context.rs:794`). That method lives in `../interpreter/`, not here; what belongs to **this** node is the memo boundary:
- **The decline path (`Ok(None)`) reads no `Machine` state and mutates none.** It derives the callee identity from the immutable `Module` — `local_callee_identity` (`../interpreter/dispatch.rs:1234-1239`): `module.functions.get(func)` plus two `module.atom_name(..)` (`beam_term/src/atom.rs:308-310`, a `Vec` index — no hash, no clone) — then asks the host. The answer is a pure function of `(Module, func index, host tables)`; `nregs` plays no part in it (it is the BIF marshalling arity only).
- **The *identity* is memoizable here; the *verdict* is not.** A per-module `func index -> resolved (owner, name, arity)` table follows the proven `ImportTarget` precedent (`../interpreter/mod.rs:93-118`: ~5 ns per resolution against ~0.5 ns cached; names stored **owned** because `JitRuntime` is `#[repr(C)]` and cannot carry the module's lifetime). Its natural owner is a sibling of `ModuleContext.labels` (`context.rs:65-72` — per-run, `Arc::ptr_eq`-deduped, dropped by `Machine::reset`) for contexts `> 0`, plus an `entry_*` twin for context `0`, which has **no** `ModuleContext`. Key on the module's `Arc` identity, never on content: a `Module::clone` mints a fresh atom table, so content-keyed entries would mix atom namespaces.
- **A cached verdict may not skip the host call.** `BudgetContext::resolve_local_call` prices one reduction per local call (`beam_rt/src/runner/budget.rs:482-485`) — load-bearing slice accounting — and the shipped host is already a single `&str` compare for every non-`prim_tty` module (`beam_rt/src/native/prim_tty.rs:123-125`), so there is nothing left to save there. Never bake a verdict into compiled code: `JitBackend::compile(&mut self, module: &Module)` is handed no `ExecutionContext` (`../jit/mod.rs:245`), and the cross-run `CodeCache` is content-keyed and shared across runs, machines and hosts.