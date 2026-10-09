# `beam_rt::runtime` — the `Runtime` façade, the scheduler-pass family and the `ProcessRunner`

## Intent
`../runtime.rs` is the `Runtime` façade: the one type that owns every runtime table and the runtime-level operations the CLI, the boot path and the `-eval` evaluator call (`run_function`/`run_module`/`run_scheduled`, the spawn API, `system_info`, `eval`, `format_term`, `refs_mut`, the timer/distribution accessors).
`../runtime/scheduler.rs` (+ `scheduler/tests.rs`) is the **scheduler-pass family**: it drives processes, ports, timers and distribution in bounded steps.
`../runner/` (a sibling directory) is the `beam_process::ProcessRunner` each scheduled process runs through; it is documented here because it is the scheduler's own instrument.
`gc.rs` holds the runtime-level GC state, accounting and safety valves, and `ets_owner.rs` the `ProcessExitObserver` host with the two owner-death drains.

## API Surface
### `runtime.rs` — the façade
`Runtime` owns every table (config, atom table, processes + mailboxes, BIF heaps, scheduler + runner state, ETS/port/NIF registries, refs, timers, the node-global `persistent_terms` store, dist) and the scratch `info_heap`.
The **scheduler-pass family** (`run_one_slice`/`run_slices`/`run_scheduler`/`pump_scheduler`/`run_scheduled`, the shared pass body, wake absorption, the port/timer/distribution halves) lives in `src/runtime/scheduler.rs` (+ `tests.rs`), but every entry point stays an inherent `Runtime` method.
* **boot/config**: `new(RuntimeConfig)`, `with_defaults()`, `boot()` (idempotent; A.7), `boot_from_script(path)` (A.7), `is_booted()`, `config()`, `load_module(path)`, `module(name)`, `preloaded()`, `preloaded_loaded()`/`is_preloaded_loaded(name)`.
* **the slice**: `run_function(path, function, arity, args) -> Result<Eterm>` (x(0)); `run_module(module, function, arity, args, out: &mut dyn OutputSink)`.
* **the CLI `run` action**: `run_scheduled(path, function, arity, args) -> Result<Eterm>` loads the file, `spawn_process`es the export and pumps the **full** scheduler in bounded steps (`RUN_STEP_SLICES` = 1000, `TimerWait::AtMost(PORT_POLL_SLICE)`) until the process returns (`x(0)`), **raises** (`take_process_error` returns the original `BeamError`), is quiescent (empty run queue, no live port, no pending timer), or outlives `RUN_DEADLINE` (60 s); exit-code classification matches the single-pass path.
* **spawning/scheduling**: `spawn_process(module, function, args)`, `spawn_fun_process(fun, args)` (`spawn/1`), `enqueue_process(pid)` (idempotent), `process_result(pid)`, `process_exit(pid)`, `run_queue_len()`.
* **refs/introspection**: `refs_mut() -> &mut RefAllocator` (**THE** ref id space — `make_ref` + monitors + timers), `system_info(key)`, `system_info_value(key)` (heap-free source of truth), `eval(expr)` (A.6 `-eval`), `format_term(term, module)`.
* **timers/dist**: `timers_mut() -> &mut Timers`, `dist_node()`, `node_name()`; the scheduler half (`fire_due_timers`, `note_fragment_wake`, `pump_dist`) is in `scheduler.rs`.

### `runner/` — the `beam_process::ProcessRunner`
`RuntimeRunner<'a>{runtime, state}` implements `ProcessRunner::run(pid, table, budget) -> Result<RunOutcome>`: `Returned(v)` → record `v` + `Exit(normal)`; `Wait{func,pc}` → park + `Wait`; `Err(e)` → record the reason term + the original `BeamError` in `RunnerState`, then `Exit(<reason term>)`. `RunnerState` holds an engine per pid plus the `results`/`exits` maps. A `Wait` **arms a receive deadline** when the parked `wait_timeout`'s delay is statically readable (`Operand::Integer`/`Literal` or a live X-register); a fired deadline resumes at `timeout` delivering nothing and a message-wake cancels it.
Split by responsibility: `runner/entry.rs` (`ProcessEntry`/`ProcessEngine`/`RunnerState`), `runner/run.rs` (`RuntimeRunner` + its `ProcessRunner` impl — one reduction slice), `runner/budget.rs` (`BudgetProbe`/`BudgetContext`), `runner/reason.rs` (`normal`/`exit_reason`/`parked_receive_timeout`/`timeout_delay`), `runner/perf.rs` (`record_baseline_code`) and `runner/tests.rs`; `runner/mod.rs` re-exports the three cross-crate names (`ProcessEntry`, `RunnerState`, `RuntimeRunner`).

### `gc.rs` — the runtime-level GC state
`GcStatsSnapshot` (`gc_stats`) reports collections/failures/halted pids/valve trips; `GcState` holds the **pin** set (a pid whose heap must not move), the **halt** set (a pid whose collection failed, or whose root-set sizes changed) and the global **valve** (`disabled`, plus the first reason and a trip counter).
`stabilize_escaped(term)` copies a runtime-level store's term when a heap may move and trips the valve when it cannot; `pin_process`/`halt_process_gc`/`disable_gc`/`gc_enabled`/`gc_process_enabled(pid)` are the mutators/queries the scheduler and the stores use.

### `ets_owner.rs` — the `ProcessExitObserver` host and the two drains
`OwnerIndex` (additive, shared with `Runtime`) records, in death order, the dead pids waiting for a drain and the append-only `(PortId, MonitorRef)` `PortWatch` pairs.
`EtsExitObserver` is the `beam_process::ProcessExitObserver`: it pushes the dead pid and its drained `PortWatch` pairs and returns (no BIF borrows the index).
`Runtime::drain_ets_owner_exits` discharges the ETS obligations (delete a dead owner's indexed tables, honour an installed heir), and `Runtime::drain_port_monitor_exits` sweeps the dangling incoming port monitors; both take the borrow with `try_borrow_mut` and **defer** to the next drain on a live borrow — never a panic, never a dropped obligation.

## Constraints
* **`unsafe`-free** (`#![deny(unsafe_code)]`) and return-based errors ([`../../CONTEXT.md`](../../CONTEXT.md) §3): every fallible API returns `beam_common::Result`.
* **The scheduler moves the table (and the runner state) out of the `Runtime`.** A pass needs `&mut Runtime` **and** `&mut ProcessTable` at once, but the table is a *field*, so `run_pass` moves both out (`take_processes`/`put_processes`) and puts them back whatever the outcome; the runner likewise takes the pid's `Machine` out of its `HashMap`.
* **A process's exit reason is not its return value** — see the parent node's Design Decisions.
* **Heap lifetime**: `Runtime` owns the process heap so a returned `Eterm` stays valid after `run_*` returns; `system_info`/`format_term` use the separate `info_heap`, reset per call.
* **There is no native (Rust-serviced) process kind.** `ProcessEntry` (`../runner/entry.rs`) has exactly two variants — `Export{module,function,arity}` and `Fun{fun,target}` — and `RuntimeRunner::run` requires a registered `ProcessEngine` (otherwise `error::unknown_process_entry`). A Rust-native service process that services its own inbox from the scheduler pass (e.g. a hand-written `file_server_2`) therefore needs an additive third entry variant + a `RuntimeRunner::run` arm + a step in the pass body (`scheduler_pass`, `scheduler.rs`); every existing runtime-owned service is either an interpreted process or a `Runtime`-level table (group leaders, the `init` stand-in).
* **Files ~1000 lines**, split by responsibility; every public item documented (`#![warn(missing_docs)]`).

## Test Strategy
* `scheduler/tests.rs`: the bounded pass family (step budgets, wake absorption, the port/timer/distribution halves) over a real `Runtime`.
* `gc.rs` units: the pin/halt/valve sets, `gc_stats`, and `stabilize_escaped` both on a movable heap and on a tripped valve.
* `ets_owner.rs` units: append-only port-watch recording, the take-for-a-drain contract, and the two drains (incl. the deferred-borrow path).
* `../runner/tests.rs`: the one-slice `ProcessRunner` contract, `RunnerState` bookkeeping, receive-deadline arming and the exit-reason mapping.
* Integration (`../../tests/`): `port_wake.rs` (a port wake resumed through the scheduler), `proc_lib_start.rs` (the cross-module `receive` suspend seam), `position.rs`, `boot.rs`, and the `run`-action suites (`cli.rs`, `slice.rs`).

## Routing Table
* `../runtime.rs` → the `Runtime` façade (boot wiring, code server + module cache, the slice, the spawn API, the timer/distribution accessors, `stabilize_term`/`materialize_args`, `system_info`, `eval`, `format_term`, `refs_mut`).
* `scheduler.rs` (+ `scheduler/tests.rs`) → the scheduler-pass family (one pass = processes → ports → timers → distribution, in bounded steps).
* `gc.rs` → `GcState` + `gc_stats`/`GcStatsSnapshot` + the pin/halt/valve sets + `stabilize_escaped`.
* `ets_owner.rs` → the `ProcessExitObserver` host (`OwnerIndex`/`EtsExitObserver`) + `Runtime::drain_ets_owner_exits`/`drain_port_monitor_exits`.
* `../runner/` → the `ProcessRunner` (`mod.rs`/`entry.rs`/`run.rs`/`budget.rs`/`reason.rs`/`perf.rs`/`tests.rs`), re-exporting `crate::runner::{ProcessEntry, RunnerState, RuntimeRunner}`.
* `../context/gc.rs` → the engine's heap-admission point (`ExecutionContext::ensure_heap`) that drives this state.
* `../boot/CONTEXT.md`, `../cli/CONTEXT.md`, `../info/CONTEXT.md` → the boot, A.6 and A.8 surfaces layered on the façade.
