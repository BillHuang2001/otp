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
may be used, or the interpreter must be. The **default is the interpreter**; the JIT is opt-in.

## API Surface (crate-internal, `pub(crate)`)
* [`entry`] — `ProcessEntry` (the runtime-scheduled entry: an export or a fun) and `RunnerState`
  (per-pid engine state, recorded `result`/`exit`, `jit_mode` override, `jit_safety` memo).
* [`run`] — `RuntimeRunner`: the `ProcessRunner` implementation (one reduction slice).
* [`budget`] — the slice's reduction budget and the wrapper context that charges it.
* [`tier`] — `JitMode` (`Interpreter` (**the `#[default]`**) / `Auto` / `Baseline`),
  `mode_from_value(Option<&str>)` (**the pure** `BEAM_RT_TIER` value semantics),
  `env_default()` (the memoized unset-variable default), `select_entry_tier(...)` and
  `jit_is_switch_safe(runtime, module)` — the switch-safety predicate.
* [`reason`] — the exit-reason translation and the finite-`receive` deadline recovery.
* [`perf`] — the `+JPperf` code-map feed (contract A.8).

## The `BEAM_RT_TIER` modes (`mode_from_value` is the single definition)
| value | mode | meaning |
|---|---|---|
| unset | `Interpreter` | **the default**: the always-correct reference tier, no measurable regression |
| `auto` | `Auto` | the JIT exactly where `jit_is_switch_safe` proves the entry module switch-free; the interpreter otherwise — **how the JIT is opted into** |
| `baseline` | `Baseline` | forced JIT (a measurement/debug override of the predicate; see the handoff below) |
| `interpreter`, anything else | `Interpreter` | an unknown value resolves to the safe default, never an error |

`JitMode`'s derived `Default` is `Interpreter`, so omitting the mode anywhere can never select the JIT;
`env_default()` memoizes the environment resolution in a `OnceLock` (one `getenv` per process).

## How the tier is selected
In [`run`]'s slice, **before** the `RuntimeContext` borrow (the predicate needs
`runtime.registry`/`runtime.nifs`, which the context borrows mutably):

```text
tier_mode = state.jit_mode()                       // Some(JitMode) or env_default()
tier      = match engine.parked { Some(_) => Tier::Interpreter,          // a resume has no tier entry
                                  None    => select_entry_tier(runtime, state, &engine.entry, tier_mode) }
```

`tier` is passed to **both** first-entry sites: the export entry (after `enter_run`, the export's own
label) and the resolved local-fun entry. `JitMode::Auto` selects `BaselineJit` iff `jit_is_switch_safe`
proves the entry module free of a module-context switch (see `## Known Issues`); `Baseline` forces the
JIT (an explicit override of the predicate, a measurement/debugging knob, never chosen by `Auto`).
A `ProcessEntry::Fun` whose local target is not resolved yet is conservatively the interpreter (its
module is only known inside the context borrow). The mode resolves from `state.jit_mode` if set
(tests), else the process-wide `BEAM_RT_TIER` default.

## The switch-safety predicate (soundness)
[`jit_is_switch_safe`] holds iff the module contains **no `Instr::Apply`/`Instr::ApplyLast`** and
**every** `Instr::CallExt`/`CallExtOnly`/`CallExtLast` target is *provably synchronous*. The only
producers of `Step::Enter` are those `CallExt*` (statically-known import) and `Apply*` (dynamic target)
instructions — verified against `crates/beam_engine`: `CallExt*` through
`interpreter::resolve_external_entry`, `Apply*` through `families::funs::apply_target`, and the two
unwind/continuation producers (`Machine::continuation_step`, the `catch`-landing at
`machine/catch.rs`) fire `Enter` only when crossing a module context the run is *already* in
(unreachable when no instruction ever switched it out of its entry module). `CallFun`/`CallFun2` go
through the synchronous `apply_fun` and never switch.

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

## Measured: why the default is the interpreter
Release build (`cargo build --release -p beam_rt`), **`$ERTS_STDLIB_DIR` set** so no per-process `erl`
start-up is included, `beam_rt run <mod>.beam <f>/<a> <arg>` alternating between
`BEAM_RT_TIER=interpreter` and the default, **min / median wall seconds**:

| workload (module compiled with the system `erlc`) | shape (from `erlc -S`) | interpreter | `auto` (→ JIT) |
|---|---|---|---|
| `walk:cnt/1 20000000` | tail loop, **no** `test_heap` | **16.05 / 17.05** | 0.116 / 0.127 |
| `walk:sum/1 20000000` | tail loop, no `test_heap` | 14.95 / 16.07 | 0.096 / 0.116 |
| `al:tailnoalloc/1 20000000` | tail loop, no `test_heap` | 19.35 | 0.114 |
| `fibn:fib/1 27` | allocating tree recursion | 0.384 / 0.403 | **0.273 / 0.284** |
| `al:list/1 3000000` | tail loop **with** `test_heap` | 1.34 / 1.40 | 1.27 / 1.43 |
| `../../beam_loader/tests/fixtures/slice.beam slice:fib/1 300000` | bignum loop | 0.135 / 0.183 | 0.119 / 0.226 |
| `… slice:fib/1 32` | bignum loop, trivial | 0.026 / 0.033 | 0.022 / 0.029 |

Contaminated with the per-process `erl` start-up (i.e. `$ERTS_STDLIB_DIR` unset, ≈150 ms added to
every run): `slice:fib/1 30` interpreter **0.276/0.322** vs `auto` 0.212/0.343, `slice:add/2 2 3`
0.321/0.342 vs 0.320/0.380, `walk:cnt/1 2000000` 1.831/1.925 vs 0.244/0.260 — the earlier
"the JIT is ~1.8× *slower* on `slice:fib/1 30`" reading was this start-up plus a debug build, not the
tier.

**Conclusion.** The tier is taken on a process's **first** entry only, and a pre-empted process is
**resumed on the interpreter** (a tier-aware resume does not exist yet), so a module whose hot loop
reaches a heap-admission point (`test_heap`/`allocate_heap`) spends all but its first slice interpreted
and the tier is ≈neutral. The baseline JIT's large win is confined to a hot loop with **no**
heap-admission point anywhere, which stays inside the one tier-chosen slice. On the objective's named
switch-free fixture (`slice.beam`) the JIT is not clearly faster (≈1.0–1.15×, inside the run-to-run
noise) and it is never beyond-noise slower anywhere — but its 139–170× win needs the narrow shape above,
and the two seams that would widen it (`beam_engine`'s `Step::Enter` deopt and a tier-aware resume) are
missing. A slower-by-default runtime is unacceptable and the JIT cannot be proven a win on the named
fixture, so the **default is the interpreter** and `BEAM_RT_TIER=auto` is the documented opt-in,
CI-checked corpus-correct by the differential gate.

## Constraints
* **`#![deny(unsafe_code)]`**; no `unwrap()` on runtime data; typed `beam_common::Result` errors.
* The tier predicate must run **before** the `RuntimeContext` borrow (it reads `runtime.registry`/
  `runtime.nifs`, which the context borrows mutably).
* A test forces a tier by setting `state.jit_mode = Some(..)` — never `std::env::set_var` (edition 2024
  makes it `unsafe`). `BEAM_RT_TIER` is the CLI-facing equivalent, and its semantics are pinned through
  the pure `mode_from_value` (not through the environment).
* `JitMode::Baseline` deliberately bypasses the predicate; it exists only for measurement, and against a
  module that can report `Step::Enter` it hits the JIT driver's typed error (which on this path becomes
  the process's exit reason).

## Test Strategy
* `#[cfg(test)]` units in [`tests`]: `select_entry_tier`/`jit_is_switch_safe` over crafted `Module`s and
  real fixtures (slice → baseline; a loadable cross-module callee → interpreter; an `apply` → interpreter;
  floor/registered/NIF targets safe; an unresolvable atom, an out-of-range `ImportRef` and an
  arity mismatch unsafe; a no-`call_ext` module safe; a forced mode overrides the predicate; the memo
  agrees with a fresh check); plus `the_unset_default_resolves_to_the_interpreter`, which pins the
  `BEAM_RT_TIER` value semantics and `JitMode::default()` through the pure `mode_from_value`.
* End-to-end: `slice:fib/1 10` through the scheduled path (`RuntimeRunner`) records `55` under both an
  explicit `JitMode::Auto` (→ baseline) and `JitMode::Interpreter`, and the scheduler path records it
  under whatever mode the environment selects.
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
  This is why the predicate exists and why the default is the interpreter.
* **No tier-aware resume: the tier applies to a first entry only.** A process that yields (the
  `budget`/`BudgetContext` heap-admission point) re-enters through `Machine::resume_outcome`, which has
  no tier parameter, so every resumed slice runs on the interpreter. That is what confines the JIT's
  benefit to loops with no heap-admission point (see the measurements). A `resume_outcome_with_tier`
  (plus a reduction hook for non-allocating loops — `ExecutionContext::reduce`, which `BudgetContext`
  does not currently charge) would extend it; both are `beam_engine`/budget-side seams.
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
* `tier.rs` → the tier mode + its `BEAM_RT_TIER` semantics (`mode_from_value`), the environment default,
  the selection and the switch-safety predicate.
* `reason.rs` / `perf.rs` → the exit-reason translation / the `+JPperf` code map.
* `tests.rs` → the unit + scheduled-path tests above.
