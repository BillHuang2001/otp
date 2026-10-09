# `beam_rt::runner` — the process runner behind the scheduler (tier selection · slice execution · result recording)

## Intent
The concrete [`ProcessRunner`](beam_process::ProcessRunner) that `beam_process`'s scheduler drives: one
reduction slice of one scheduled process. It builds a [`RuntimeContext`](crate::context::RuntimeContext)
that runs **as that pid** in the caller-supplied process table, takes the pid's engine state out of
[`RunnerState`], executes it (`run_export_outcome_with_tier` for a first run,
`resume_outcome` for a process parked at a blocking `receive`) and records the observable result
(the return value on the pointer-stable arena, or the raise's reason term).

This node additionally owns **static execution-tier selection**: a process's *first* entry may run on
either tier ([`beam_engine::Tier`]), and [`tier`] decides — soundly — whether the native baseline JIT
may be used, or the interpreter must be.

## API Surface (crate-internal, `pub(crate)`)
* [`entry`] — `ProcessEntry` (the runtime-scheduled entry: an export or a fun) and `RunnerState`
  (per-pid engine state, recorded `result`/`exit`, `jit_mode` override, `jit_safety` memo).
* [`run`] — `RuntimeRunner`: the `ProcessRunner` implementation (one reduction slice).
* [`budget`] — the slice's reduction budget and the wrapper context that charges it.
* [`tier`] — `JitMode` (`Auto` (default) / `Interpreter` / `Baseline`), `env_default()` (the
  `BEAM_RT_TIER` environment default, read once), `select_entry_tier(...)` and
  `jit_is_switch_safe(runtime, module)` — the switch-safety predicate.
* [`reason`] — the exit-reason translation and the finite-`receive` deadline recovery.
* [`perf`] — the `+JPperf` code-map feed (contract A.8).

## How the tier is selected
In [`run`]'s slice, **before** the `RuntimeContext` borrow (the predicate needs
`runtime.registry`/`runtime.nifs`, which the context borrows mutably):

```text
tier_mode = state.jit_mode()                       // Some(JitMode) or env_default()
tier      = match engine.parked { Some(_) => Tier::Interpreter,          // a resume has no tier entry
                                  None    => select_entry_tier(runtime, state, &engine.entry, tier_mode) }
```

`tier` is passed to **both** first-entry sites: the export entry (after `enter_run`, the export's own
label) and the resolved local-fun entry. `JitMode::Auto` (the default) selects `BaselineJit` iff
`jit_is_switch_safe` proves the entry module free of a module-context switch (see `## Known Issues`);
`Interpreter`/`Baseline` force a tier (`Baseline` is an explicit override of the predicate, a
measurement/debugging knob, never chosen by `Auto`). A `ProcessEntry::Fun` whose local target is not
resolved yet is conservatively the interpreter (its module is only known inside the context borrow).
The mode resolves from `state.jit_mode` if set (tests), else the process-wide `BEAM_RT_TIER` default
(`interpreter` / `baseline`, anything else → `Auto`).

## The switch-safety predicate (soundness)
[`jit_is_switch_safe`] holds iff the module contains **no `Instr::Apply`/`Instr::ApplyLast`** and
**every** `Instr::CallExt`/`CallExtOnly`/`CallExtLast` target is *provably synchronous*. The only
producers of `Step::Enter` are those `CallExt*` (statically-known import) and `Apply*` (dynamic target)
instructions — verified against `crates/beam_engine`; `CallFun`/`CallFun2` go through the synchronous
`apply_fun` and never switch, and the two unwind/continuation producers only fire when crossing a
module context the run is *already* in (unreachable when the run never left its entry module).

"Provably synchronous" is proven only from the three cheap `Ok(None)` short-circuits of
`RuntimeContext::resolve_external_call` (evaluated in the seam's own order, with the seam's own
arguments — the **import** `{module, function, arity}`, not the instruction operand): the floor
namespaces ([`crate::native::FLOOR_MODULES`]), a module with a loaded NIF
([`crate::nif::NifRegistry::is_loaded`]), or a registered `beam_bifs` key
([`beam_bifs::BifRegistry::contains`]). The seam's other `Ok(None)` cases (load failure, float guard,
call-site observer, unknown export) can only add safety, so the predicate is **conservative**: it may
refuse the JIT for a module that would in fact have run fine, but it never permits one that could
switch. An already-borrowed NIF registry (`try_borrow` err) is treated as **not safe** (never a panic).

The verdict is **sticky in the safe direction** (`FLOOR_MODULES` is constant; `load_nif` and the
registry only ever *add*), which is what makes the per-module `jit_safety` memo sound. A stale *unsafe*
verdict only costs the faster tier (the interpreter is always correct).

## Constraints
* **`#![deny(unsafe_code)]`**; no `unwrap()` on runtime data; typed `beam_common::Result` errors.
* The tier predicate must run **before** the `RuntimeContext` borrow (it reads `runtime.registry`/
  `runtime.nifs`, which the context borrows mutably).
* A test forces a tier by setting `state.jit_mode = Some(..)` — never `std::env::set_var` (edition 2024
  makes it `unsafe`). `BEAM_RT_TIER` is the CLI-facing equivalent.
* `JitMode::Baseline` deliberately bypasses the predicate; it exists only for measurement.

## Test Strategy
* `#[cfg(test)]` units in [`tests`]: `select_entry_tier`/`jit_is_switch_safe` over crafted `Module`s and
  real fixtures (slice → baseline; a loadable cross-module callee → interpreter; an `apply` → interpreter;
  floor/registered/NIF targets safe; an unresolvable atom, an out-of-range `ImportRef` and an
  arity mismatch unsafe; a no-`call_ext` module safe; a forced mode overrides the predicate; the memo
  agrees with a fresh check).
* End-to-end: `slice:fib/1 10` through the scheduled path (`RuntimeRunner`) records `55` under **both**
  `JitMode::Auto` (→ baseline) and `JitMode::Interpreter`.
* Crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt`.

## Known Issues / Handoffs
* **`beam_engine`'s baseline JIT driver lacks a deopt seam for `Step::Enter`.** `jit::baseline`'s driver
  (`run_compiled_outcome`, `jit_step_one`) deopts to the interpreter *step* for an unlowered
  instruction, but when that step reports `Step::Enter` (a module-context switch) it returns the hard
  typed `unsupported("jit: module context switch outside the interpreter driver")` — the
  block/label-map/literal-area are all bound to one module. On the runner path that error *becomes the
  process's exit reason*, i.e. a wrong observable result vs the oracle. A driver able to hand a
  `Step::Enter` off to the **multi-module** interpreter driver would let the JIT run any module and
  render this static predicate unnecessary. **Owner: `beam_engine`** (out of this node's write scope).
* Consequently the JIT is only reached for switch-free modules: `slice.beam` (imports only
  `erlang:get_module_info/1,2`) does; a workload that calls a loadable cross-module `.beam`
  (e.g. real Cowboy code, or `tools/bench`'s `list_walk`) does **not** and correctly stays on the
  interpreter.

## Routing Table
* `mod.rs` → module docs, sub-module wiring, root re-exports (`ProcessEntry`, `RunnerState`,
  `RuntimeRunner`).
* `entry.rs` → `ProcessEntry` + `RunnerState` (the `jit_mode`/`jit_safety` fields and the `jit_mode()`
  accessor).
* `run.rs` → `RuntimeRunner`: the tier selection + the first-entry/resume execution.
* `budget.rs` → the reduction budget + the charging wrapper context.
* `tier.rs` → the tier mode, the environment default, the selection and the switch-safety predicate.
* `reason.rs` / `perf.rs` → the exit-reason translation / the `+JPperf` code map.
* `tests.rs` → the unit + scheduled-path tests above.
