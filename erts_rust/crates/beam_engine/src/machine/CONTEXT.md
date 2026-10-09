# `beam_engine` — Machine Core (`src/machine/`)

## Intent
The process-state core of the `beam_engine` interpreter: the x-register file, the float-register (FR) file, the Erlang stack (Y slots), the call-frame stack and the exception catch-frame stack, plus the module-context registry and the run/resume/drive entries.
It reproduces the C interpreter's frame model (`../../../../erts/emulator/beam/emu/macros.tab`) with each frame's continuation-pointer slot modeled as a typed `Continuation` instead of a raw stack word.
The interpreter dispatch loop and every instruction family build on these primitives; the module is split by responsibility and re-exports every name as `crate::machine::…`, so the split is invisible to callers.

## API Surface
- `Machine` — the process state (fields are private to this module tree).
  - `x(n)->Option<Eterm>`, `set_x` (crate), `get_fr(n)`, `set_fr`; `MAX_REGISTER = 1024`, `MAX_FLOAT_REGISTER = 1024`; `reset()` refills both files in 128-byte chunks and never reallocates in steady state.
  - `y`/`y_mut`/`set_y`, `push_frame`/`pop_frame`/`pop_frame_checked`/`trim`, `continuation`/`set_continuation`/`finish`, `frame_depth`/`stack_depth`, `resumption_contexts`/`resumption_addresses`.
  - Heap seam: `ensure_heap(heap_need, live, ctx)`, `alloc_rooted(ctx, n, live)`, `with_roots(live, f)`, `x_mut`/`stack_mut`/`fr_mut`.
  - Exception scaffolding: `push_catch_frame`/`pop_catch_frame`/`pop_catch_frame_checked`, `CatchFrame`, `throw`/`throw_error`/`throw_with_trace`/`throw_with_stacktrace`, `pending_exception`/`pending_class`, `raise_stacktrace`, `set_class_atom`.
  - Runs: `run_export`/`run_label` (non-blocking; fold a `Wait` into `error::blocked()`), `run_export_outcome`/`run_label_outcome`/`resume_outcome` (+ the `*_with_tier` tier entries), `RunOutcome { Returned, Wait }`, `drive`, `reductions`/`reset_reductions`.
  - Module contexts: `register_context`/`switch_context`/`module_context`/`current_context`, `set_literals`, `literal(index)`.
  - Introspection: `current_code`/`entry_call`/`current_function`/`initial_call`/`current_location`/`current_stacktrace`/`current_module`/`wait_deadline`/`timeout_delay`.
  - Off-heap binaries: `alloc_refc_binary` (held in `refc_binaries`, which `reset` never clears).
- `RunOutcome`, `WaitDeadline`, `CatchFrame`, `MAX_REGISTER`/`MAX_FLOAT_REGISTER` are re-exported at `crate::machine::…`.
- The frozen root set type `RootSet` lives in the sibling `../context.rs`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; an out-of-range register/frame/label is a typed error, never a panic.
- Term construction/inspection/compare go through `beam_term` only.
- `reset` clears both register files, the stack, the frame/catch stacks, the pending exception, the class atoms, the literal binding, the module-context registry, `current_code`/`entry_call` and `trace_on`. It does NOT clear `refc_binaries` (a returned term may outlive the run).
- `resume_outcome` deliberately does NOT reset: a suspended cross-module frame keeps its context id and the callee module handle it must be resumed in.
- A raise reads the innermost catch frame but never pops it (`try_case`/`try_end`/`catch_end` consume exactly one entry).

## Root-set / GC contract (frozen — VERIFIED against the code)
- `Machine::ensure_heap` (`frames.rs:283`) hands the WHOLE `RootSet` — `x[0..live)`, the ENTIRE Erlang stack and the ENTIRE FR file — to `ExecutionContext::ensure_heap_full`; the trait default forwards to the narrow `ensure_heap(heap_need, live, x, stack)` with the FR file discarded, so a host that predates the full door is byte-identical and a host with a collector sees every root.
- `Machine::alloc_rooted`/`with_roots` (`mod.rs:338,363`) hand the same whole set to `ExecutionContext::alloc_words_rooted`; the trait default discards the roots and forwards to `alloc_words`.
- `live` is only a hint (`RootSet::ALL_LIVE` = all), clamped to the file length.
- **GAP (owner: the `terms`/`families` call sites, not this node):** the ordinary term builders allocate through the **unrooted** `alloc_words`, so nothing on the normal path ever triggers a collection. Exact call sites: `../terms/construct.rs:47` (`make_tuple`), `:68` (`make_cons`), `:136` (`make_heap_binary`); `../terms/number.rs:34` (`make_float`), `:63` (`make_integer`); `../terms/map.rs:46` (`make_flatmap`), `:72` (`make_map`); `../terms/fun.rs:75` (`make_fun`); plus `../families/bitstrings/raw.rs:184,227,272,326,360` and `../families/records/mod.rs:834,965`.

## Known Issues
- `Contexts::len` (`context.rs:116`) carries `#[allow(dead_code)]` — a minor test-only accessor.
- A module context is identified by `Arc::ptr_eq`; two equal-content modules at different allocations are two contexts (wasteful but never wrong).

## Test Strategy
- In-module `#[cfg(test)]` unit tests in every file (registers, frames, catch, run, location) plus the crate-level integration suites; run via `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine`.

## Routing Table
- `registers.rs` → x/FR files, bounds, `reset`.
- `frames.rs` → `Continuation`/`Frame`, Y slots, push/pop/trim, `ensure_heap`, caller-chain introspection.
- `catch.rs` → `CatchFrame`, the raise/unwind entries, the landing convention, `raise_stacktrace`.
- `run.rs` → run/resume/tier entries, `RunOutcome`.
- `drive.rs` → `Machine::drive` (the module-context-aware loop) and reduction accounting.
- `context.rs` → the `ModuleContext`/`Contexts` registry.
- `location.rs` → execution-position introspection + `WaitDeadline`.
- `../context.rs` → the `ExecutionContext`/`Mailbox` seams + `MinimalContext` + `RootSet` (sibling file).
- `../interpreter/` → the dispatch loop + `TestKind` + BIF arm + A.8 trace (sibling; its `CONTEXT.md` records the `IsEq` and `is_boolean` divergences).
- `../terms/` → term builders/readers (sibling; its `CONTEXT.md` records the GC-rooting gap and the `beam_term` duplication).
