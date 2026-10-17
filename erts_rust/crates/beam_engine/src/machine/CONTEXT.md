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
  and the **in-machine BIF-dispatch gate** `bif_in_machine_on` — the cached
  `ExecutionContext::wants_bif_in_machine` answer, latched once per run by `Machine::drive` **and** by the
  native baseline tier's driver, read by `Machine::dispatch_bif_call`); `MAX_REGISTER`/`MAX_FLOAT_REGISTER`; `reset`; the **root-set hand-off**
  (`stack_mut`, `with_roots` handing over the whole `RootSet`, and `alloc_rooted` — the convenience
  wrapper that calls `ExecutionContext::alloc_words_rooted`); and the **off-heap refc-binary handle view**
  `refc_binaries()` — the read-only slice of the `RefcBinary` payload handles `alloc_refc_binary` built,
  which a host clones into a copy's **source** off-heap table so the copy engine can resolve a refc
  binary's payload by data pointer (a cloned handle's refcount keeps the payload alive past the machine).
- `registers.rs` — x/FR access (`get_fr`/`set_fr`, bounds-checked → `bad_register`; `x_mut`/`fr_mut` as
  the single-view root-set accessors); `fr_mark()` — the **live leading FR-slot count** (the FR high-water
  mark, `0` ⇒ no live FR slot), the FR twin of the `x[0..live)` root-set bound, so a host can publish/scan
  only the slots an FR write actually touched (exact after the ordinary interpret seams; a native JIT block
  or a caller that took a raw `fr_mut`/`with_roots` slice sets the crate-internal `regs_escaped` flag and may
  understate it — the caller must account for that itself); `Machine::reset` clears the raise snapshot
  alongside the module contexts (both belong to the run that is starting) **and clears all four raise
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
- `location.rs` — execution-position introspection (`current_code`, `current_function`, `current_location`,
  `initial_call`, `current_stacktrace`, `current_module`/`active_context`) **and** the parked-`receive`
  deadline seam `Machine::wait_deadline`/`timeout_delay` + `WaitDeadline { delay_ms, resume }`: it reads the
  `wait_timeout` at `current_code` (the blocked instruction, exact because a blocking op is never JIT-inlined)
  in the module `active_context()` names — the callee's own module when the park is inside a cross-module
  callee (context != 0), else the passed entry module — and reports the `timeout` continuation (`pc + 1`) a
  host re-enters when its own timer fires. The engine arms no timer: a non-zero `wait_timeout` just suspends.
- `run.rs` — `run_export`/`run_label` (non-blocking) and `run_export_outcome`/`run_label_outcome`/`resume_outcome`
  (blocking-aware); the **tier-selecting** entries `run_with_tier`/`run_export_outcome_with_tier`/
  `run_label_outcome_with_tier` (first entry) and `resume_outcome_with_tier` (**resume** — a slice that suspended on a
  native tier resumes on it: `Interpreter` → `resume_outcome`, `BaselineJit` → the cached no-reset baseline resume,
  `Cranelift` → a typed error); `RunOutcome` (`Returned` / `Wait`), re-exported at the crate root.
- `drive.rs` — **`Machine::drive`**: owns the `(func, pc)` position *and* the module context those indices belong
  to, folds every `Step` back into that position, and charges **exactly one reduction per interpreted instruction
  before the instruction's effect** through the defaulted `ExecutionContext::reduce` (the only pre-emption point a
  non-allocating loop reaches; read back through `Machine::reductions()`/`reset_reductions()`). The charge precedes
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
  every live caller innermost first, plus the guard that a caught raise lands unchanged. `test_support.rs`
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
- `reset` clears the register files, stacks, catch frames, contexts, the raise snapshot, the pending exception and
  the trace gate, but **not** the `refc_binaries` arena; a resume (`resume_outcome`) resets nothing, so the
  contexts and the raise snapshot both survive it.

## See Also
- `../interpreter/CONTEXT.md` → the per-instruction dispatcher (`step_instruction`, also the JIT deopt target).
- `../CONTEXT.md` → the engine's binding execution semantics; `../jit/CONTEXT.md` → the native tiers' block ABI.

## `Machine::writable_payloads` — the writable-append registry

A small side list of `(payload base, capacity in bytes)` for the off-heap payloads this machine
allocated **for appending** (`bs_init_writable`'s backing store, C's `BIN_FLAG_WRITABLE` binary),
deposited by `Machine::mark_writable(base, capacity)` and queried by
`Machine::writable_capacity(base) -> Option<usize>` (a linear scan; no BEAM-heap allocation). The
frozen term layout carries no capacity word and `beam_alloc::RefcBinary` is immutable, so the
capacity has to live beside the machine; keying it per machine also gives C's
`erts_pin_writable_binary` property for free — a payload another process's copy points at is not in
*this* list, so it is never written in place here.

Only `mark_writable` entries are writable: an ordinary binary's payload length is never taken as a
capacity (the roster `Machine::refc_binaries` is *not* consulted). The list is **never cleared by
`Machine::reset`**, for the same reason `refc_binaries` is not — a registry entry must never outlive
the payload it names. Consumer: `families::bitstrings::raw::{writable_append_target, InPlace}`.
