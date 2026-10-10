# `beam_rt::runner` — the process runner behind the scheduler (tier selection · slice execution · result recording)

## Intent
The concrete [`ProcessRunner`](beam_process::ProcessRunner) that `beam_process`'s scheduler drives: one
reduction slice of one scheduled process. It builds a [`RuntimeContext`](crate::context::RuntimeContext)
that runs **as that pid** in the caller-supplied process table, takes the pid's engine state out of
[`RunnerState`], executes it (`run_export_outcome_with_tier` for a first run,
`resume_outcome_with_tier` — on the process's **remembered** tier — for a process parked at a blocking
`receive` or pre-empted on the reduction budget) and records the observable result
(the return value on the pointer-stable arena, or the raise's reason term).

This node additionally owns **static execution-tier selection**: a process's *first* entry may run on
either tier ([`beam_engine::Tier`]), and [`tier`] decides — soundly — whether the native baseline JIT
may be used, or the interpreter must be. The selection is **remembered on the engine**
(`ProcessEngine::tier`) and re-used by every later re-entry, so a process keeps its tier for its whole
life. The **default is `Auto`**: the JIT exactly where [`tier::jit_is_switch_safe`] proves the entry
module free of a module-context switch (`Step::Enter`), the interpreter everywhere else.

## API Surface (crate-internal, `pub(crate)`)
* [`entry`] — `ProcessEntry` (the runtime-scheduled entry: an export or a fun), `ProcessEngine`
  (per-pid machine/entry/parked state + the **remembered `tier`**) and `RunnerState`
  (per-pid engine state, recorded `result`/`exit`, `jit_mode` override, `jit_safety` memo).
* [`run`] — `RuntimeRunner`: the `ProcessRunner` implementation (one reduction slice) — the tier
  selection (once, at the first entry) and the tier-aware first-entry/resume execution, the
  parked-machine root publication (`publish_parked_roots`/`resume_from_process_roots`) and the
  recorded outcome. Both recorded terms are transported with the **machine's off-heap handles**
  (`engine.machine.refc_binaries()`, which is why they are stabilized *before* the engine goes back
  into — or is dropped out of — `RunnerState`): the return value through
  `Runtime::stabilize_term_with`, the exit reason through `Runtime::try_stabilize_escaped_with`. An
  engine-built off-heap binary (`bs_init_writable`/`bs_create_bin` above the on-heap limit) is
  therefore copied rather than refused with `badarg` (the return path) or kept un-copied against a
  dangling payload (the exit path).
* [`budget`] — the slice's reduction budget and the wrapper context that charges it.
* [`tier`] — `JitMode` (`Auto` (**the `#[default]`**) / `Interpreter` / `Baseline`),
  `mode_from_value(Option<&str>)` (**the pure** `BEAM_RT_TIER` value semantics),
  `env_default()` (the memoized unset-variable default), `select_entry_tier(...)` and
  `jit_is_switch_safe(runtime, module)` — the switch-safety predicate.
* [`reason`] — the exit-reason translation and the finite-`receive` deadline recovery.
* [`perf`] — the `+JPperf` code-map feed (contract A.8).

## The `BEAM_RT_TIER` modes (`mode_from_value` is the single definition)
| value | mode | meaning |
|---|---|---|
| unset | `Auto` | **the default**: the JIT exactly where `jit_is_switch_safe` proves the entry module switch-free, the interpreter everywhere else |
| `auto` | `Auto` | the default, named explicitly |
| `baseline` | `Baseline` | forced JIT (a measurement/debug override of the predicate; see the handoff below) |
| `interpreter`, anything else | `Interpreter` | the always-correct reference tier; an unknown value resolves to it, never an error |

`JitMode`'s derived `Default` is `Auto`, so omitting the mode anywhere selects the predicate-guarded
default — never the switch-blind `Baseline`; `env_default()` memoizes the environment resolution in a
`OnceLock` (one `getenv` per process).

## How the tier is selected
In [`run`]'s slice, **before** the `RuntimeContext` borrow (the predicate needs
`runtime.registry`/`runtime.nifs`, which the context borrows mutably):

```text
tier = match engine.parked { Some(_) => engine.tier,            // a resume re-uses the remembered tier
                             None    => { let t = select_entry_tier(runtime, state, &engine.entry, state.jit_mode());
                                          engine.tier = t; t } }
```

The tier is chosen **once**, at a process's first entry, and written to the engine
(`ProcessEngine::tier`); every later slice — a pre-emption on the reduction budget, or a `receive`
park — takes the `Some(_)` arm and resumes on that same tier. It is passed to **both** first-entry
sites — the export entry (after `enter_run`, the export's own label) and the resolved local-fun entry —
and to the resume through `Machine::resume_outcome_with_tier`, whose `Tier::Interpreter` arm delegates
to the plain `Machine::resume_outcome` (so an interpreter-tier process is byte-for-byte unchanged).
`JitMode::Auto` selects `BaselineJit` iff `jit_is_switch_safe` proves the entry module free of a
module-context switch (see `## Known Issues`); `Baseline` forces the JIT (an explicit override of the
predicate, a measurement/debugging knob, never chosen by `Auto`). A `ProcessEntry::Fun` whose local
target is not resolved yet is conservatively the interpreter (its module is only known inside the
context borrow). The mode resolves from `state.jit_mode` if set (tests), else the process-wide
`BEAM_RT_TIER` default.

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

## Parked-machine root publication (the runner↔GC hand-off)
A suspended process's machine lives in [`RunnerState::engines`], but its registers are only a *copy* of
the process's own root set. A collection driven while the process is parked — `Process::deliver`'s exit
cascade (`beam_process`) is **not** gated by `Runtime::gc_process_enabled` — relocates the process's heap
against its own `regs`, so a parked machine that had not published its roots would be collected against a
partial root set and left with a stale word in its `x`.
[`run`] therefore calls `publish_parked_roots(table, pid, machine)` at **both** park arms — the
pre-emption/`Yield` arm and the `Ok(EngineRunOutcome::Wait)` arm — right before `engines.insert`, and
`resume_from_process_roots(ctx, pid, machine)` after `ctx.enter_module(module)` when
`engine.parked.is_some()`.
Both delegate to the **one** shared implementation in [`context::gc`](../context/CONTEXT.md)
(`publish_roots`/`scatter_roots`), which the in-slice heap-admission point (`ensure_heap`) also uses, so
the park/resume and the admission point can never drift:
* `publish_parked_roots` hands the machine's roots over with `RootSet::ALL_LIVE` — the conservative
  (always-safe) choice, since a parked machine's live count is not exposed and over-approximating a
  copying collector's root set can only keep a term alive, never free a live one.
* `resume_from_process_roots` is the context's `RuntimeContext::resync_parked_roots`: it copies the
  process's authoritative root set back into the machine and, if the slices no longer match, **halts the
  process's collection** (`Runtime::halt_process_gc`) rather than run against inconsistent roots.
The runner's raise arm likewise copies the recorded exit reason through `Runtime::try_stabilize_escaped`,
so a reason that genuinely cannot be copied is the honest `error:system_limit`, never a stored dangling
word.

## Measured: why the default is the safe `auto` mode
Release build (`cargo build --release -p beam_rt`), no per-process `erl` start-up (the system code
path resolves natively from the filesystem), `beam_rt run <mod>.beam <f>/<a> <arg>` alternating between
`BEAM_RT_TIER=interpreter` and the default, **min / median wall seconds**:

| workload (module compiled with the system `erlc`) | shape (from `erlc -S`) | interpreter | `auto` (the default) |
|---|---|---|---|
| `walk:cnt/1 20000000` | tail loop, **no** `test_heap` | **16.05 / 17.05** | 0.116 / 0.127 |
| `walk:sum/1 20000000` | tail loop, no `test_heap` | 14.95 / 16.07 | 0.096 / 0.116 |
| `al:tailnoalloc/1 20000000` | tail loop, no `test_heap` | 19.35 | 0.114 |
| `fibn:fib/1 27` | allocating tree recursion | 0.384 / 0.403 | **0.273 / 0.284** |
| `al:list/1 3000000` | tail loop **with** `test_heap` | 1.34 / 1.40 | 1.27 / 1.43 |
| `../../beam_loader/tests/fixtures/slice.beam slice:fib/1 300000` | bignum loop | 0.135 / 0.183 | 0.119 / 0.226 |
| `… slice:fib/1 32` | bignum loop, trivial | 0.026 / 0.033 | 0.022 / 0.029 |

A later release re-measurement (**min of 3** alternating runs on the same `$ERTS_STDLIB_DIR` host) pins
the default on a fresh `tb` fixture: `tb:cnt/1 20000000` (non-allocating tail loop) interpreter
**10.648 s** vs the default **0.079 s**, and `tb:fib/1 27` (allocating tree recursion) interpreter
**0.296 s** vs **0.205 s**. The unset `BEAM_RT_TIER` resolves to exactly `Auto` (timings identical to
`BEAM_RT_TIER=auto`), and the FULL differential suite under the new default is
`total=1213 agree=1211 mismatch=0 unsupported=2` (A.9 `155/155` agree) — so the default is CI-checked
corpus-correct.

**Conclusion.** The tier is taken on a process's **first** entry and then carried for its whole life: a
pre-empted slice and a `receive` park both resume through `Machine::resume_outcome_with_tier` on the tier
the process was selected for. So the baseline JIT's win is no longer confined to a hot loop with **no**
heap-admission point — an *allocating* loop keeps the JIT across every slice too. Measured on a release
build, alternating runs, min of 3, on the committed `preempt:alloc_loop/1` fixture (one `test_heap` per
iteration, so it pre-empts repeatedly; `N = 200 000`): interpreter **0.180 s** vs the default
**0.086 s** (≈2.1×). The non-allocating rows above are the 139–170× shape (nothing can pre-empt such a
loop, so it stays in its one tier-chosen slice); on the switch-free fixture (`slice.beam`) the JIT is not
clearly faster (≈1.0–1.15×, inside the run-to-run noise) and it is never beyond-noise slower anywhere.
`auto` therefore neither regresses a workload nor takes a correctness risk, because the predicate only
ever *adds* the JIT where the entry module is proven switch-free. That is why the **default is the
predicate-guarded `Auto`** — never the switch-blind `Baseline`, whose slice could hit the JIT driver's
`Step::Enter` gap and turn a typed capability gap into a wrong exit reason — while
`BEAM_RT_TIER=interpreter` pins the always-correct reference tier.

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
  agrees with a fresh check); plus `the_unset_default_resolves_to_auto`, which pins the
  `BEAM_RT_TIER` value semantics and `JitMode::default()` through the pure `mode_from_value`.
* Tier persistence across a resume, proven through the per-thread **baseline compiled-block cache**
  (`beam_engine::jit::cache::{baseline_cache_len, clear_baseline_cache}`) — empty the cache between the
  park and the resume, so a resume that came back on the baseline tier is the only thing that can
  repopulate it: `a_preempted_process_resumes_on_its_baseline_tier` (`preempt:alloc_loop/1` under a tiny
  budget → `Yield`, then the resume completes the loop on the remembered tier),
  `a_receive_parked_process_resumes_on_its_baseline_tier` (`recv_after:wait_forever/0` → `Wait`, then a
  `ProcessTable::deliver_simple` message wakes it and the resume takes it on the remembered tier), and
  `an_interpreter_tier_process_is_unchanged_across_a_resume` (same loop on `JitMode::Interpreter` records
  the same result and compiles nothing on either slice). Each asserts `ProcessEngine::tier` as well.
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
  This is why the predicate exists and why the default is the guarded `Auto` mode, never the
  switch-blind `Baseline`.
* **A non-allocating loop has no admission point, so it cannot be pre-empted at all.** The budget is
  charged only at `ExecutionContext::ensure_heap` (`test_heap`/`allocate_heap`), so a loop with no
  allocation anywhere runs to its next `return`/`wait` inside the one slice that entered it — and
  therefore never leaves its tier either. `ExecutionContext::reduce` is the seam that would add a
  per-instruction admission point, and `BudgetContext` does not charge it. **Owner: `beam_engine`** (the
  seam) + `budget.rs` (the charge).
* Consequently the JIT is only reached for switch-free **entry** modules: `slice.beam` (imports only
  `erlang:get_module_info/1,2`) does; a workload whose entry module calls a loadable cross-module `.beam`
  (e.g. real Cowboy code, or `tools/bench`'s `list_walk`) does **not** and correctly stays on the
  interpreter for its whole life.

## Routing Table
* `mod.rs` → module docs, sub-module wiring, root re-exports (`ProcessEntry`, `RunnerState`,
  `RuntimeRunner`).
* `entry.rs` → `ProcessEntry`, `ProcessEngine` (incl. the remembered `tier`) + `RunnerState` (the
  `jit_mode`/`jit_safety` fields and the `jit_mode()` accessor).
* `run.rs` → `RuntimeRunner`: the (first-entry) tier selection and the tier-aware resume, the
  parked-machine root publication (`publish_parked_roots`/`resume_from_process_roots`) and the
  exit-reason stabilization.
* `budget.rs` → the reduction budget + the charging wrapper context.
* `tier.rs` → the tier mode + its `BEAM_RT_TIER` semantics (`mode_from_value`), the environment default,
  the selection and the switch-safety predicate.
* `reason.rs` / `perf.rs` → the exit-reason translation / the `+JPperf` code map.
* `tests.rs` → the unit + scheduled-path tests above.
