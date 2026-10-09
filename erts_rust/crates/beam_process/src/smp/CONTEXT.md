# `beam_process/src/smp` — the multi-scheduler (SMP) pool

## Intent
The SMP front of this crate: `N` scheduler threads over **one shared process table**, with
per-scheduler run queues plus work stealing, the cross-scheduler `ThreadProgress` barrier, the
high-level `TimerService` and the two `DirtyPool`s. It is the Rust analogue of the C scheduler
threads plus the global run queue (`erts/emulator/beam/erl_process.c`) and is what makes the
Stage-5 scheduler *concurrent* without re-implementing the (frozen) slice logic.

Crate-root re-exports: `beam_process::{SchedulerPool, SchedulerConfig, SchedulerStats, SchedulerStat,
SharedProcessTable}`.

Read the crate `CONTEXT.md` first for the contracts this module obeys (error strategy, root set,
`ProcessRunner` seam, resume-on-send). Legacy C ground truth (read-only):
`../../../../erts/emulator/beam/erl_process.c`, `erl_thr_progress.c`, `erl_hl_timer.c`,
`erl_nfunc_sched.c`.

## API Surface (FROZEN — dependents route here)
- `mod.rs`
  - `SchedulerConfig` (`Copy + Eq + Default`) — `new()` defaults: **1** scheduler, `0` dirty
    workers, timers on, `DEFAULT_REDUCTIONS_PER_SLICE`, `DEFAULT_LOCK_QUANTUM`; chainable setters
    `set_schedulers`/`set_reductions_per_slice`/`set_lock_quantum`/`set_dirty_cpu`/`set_dirty_io`/
    `set_timers` (+ matching getters), `with_schedulers(n)`.
  - `SharedProcessTable { inner: Mutex<ProcessTable> }` — the whole table behind one
    `parking_lot::Mutex`; `new(ProcessTable) -> Arc<Self>`, `lock()`, `try_lock()`. Carries two
    **documented `unsafe impl Send`/`unsafe impl Sync`** (a contained handoff — see the `Send`/`Sync`
    invariant under *Locking discipline*).
- `queues.rs` — `pub(crate) Queues`: a shared **injector** `Mutex<RunQueue>` + one `Mutex<RunQueue>`
  per scheduler index + the shared `InFlight` counter; `acquire(index)` tries own queue → injector →
  round-robin **steal of half** a peer's queue (`RunQueue::split_off_half`), returning
  `Acquired::{Pid, Stolen, Idle}`; length queries (`injector_len`/`local_len`/`local_lengths`/
  `total_len`) and the idle predicates `has_takeable_work(index)`/`is_idle()` (`total_len() == 0 &&
  inflight == 0`).
- `worker.rs` — `Worker::run` (the scheduler-thread loop): update thread progress, acquire work
  (own → injector → steal), run one slice through [`run_locked_quanta`] (below) with requeues
  **buffered** in a worker-local `buf`, flush `buf` to the local queue only after the table guard
  drops. Plus `pub(crate) PoolSignals` (work/idle condvars + a stop flag; the park predicate is
  re-checked under the signals mutex so a wakeup cannot be lost) and `record_error`.
- `stats.rs` — `SchedulerStats { schedulers, run_queue_len, slices, steals, active,
  dirty_cpu_pending, dirty_io_pending, timers_pending }` and `SchedulerStat { index, run_queue_len,
  slices, steals, active }` + the internal `WorkerCounters`/`InFlight` atomics.
- `pool.rs` — `SchedulerPool<R: ProcessRunner + Send + 'static>`: `start(config, table, make_runner)
  -> Result<Self>`; `schedulers`, `lock_quantum`, `progress`, `table`, `timer_service`, `dirty_pool`,
  `spawn_default`, `spawn(policy)`, `enqueue`/`enqueue_with_priority`, `deliver_simple`, `send_after`,
  `submit_dirty`, `stats`, `scheduler_stats`, `run_queue_len`, `run_queue_lengths`,
  `wait_idle(Option<Duration>)`, `safepoint`, `take_error`, `shutdown`, `join`; plus the
  `TimerSink`/`DirtyCompletionSink` impls. **Lifecycle:** `schedulers() == 0` is **clamped to 1**;
  `shutdown()` abandons any still-queued pids (the processes stay in the table); `join(self)` = stop
  + join scheduler threads + timer-service shutdown/join + `Arc::try_unwrap` the dirty pool and join
  it (falling back to `shutdown()` when a caller still holds a clone).
  `slices` counts **popped-pid iterations** (one per slice), not runner invocations.

### Lock granularity: the bounded-quantum table lock (CURRENT)
A worker holds the shared table lock for **at most one reduction quantum of a slice**, never for a
whole slice:

- `SchedulerConfig::set_lock_quantum(n)` (default `scheduler::DEFAULT_LOCK_QUANTUM`, clamped to ≥ 1)
  is the maximum number of reductions a scheduler runs per lock acquisition.
- `worker::run_locked_quanta(table, pid, runner, budget, quantum, enqueue)` runs the already-popped
  pid as `ceil(budget / quantum)` quanta: each quantum is one `table.lock()` +
  `ProcessRunner::run(pid, table, chunk)` + `drain_woken_into`, and the lock is **released between
  quanta**. `budget` is the per-slice `reductions_per_slice`; the process keeps the whole budget and
  stays in-flight (out of every run queue) for the entire slice.
- Schedule-in/schedule-out are the frozen `scheduler::{slice_begin, slice_end}` pair the
  single-threaded `Scheduler` also uses (`run_one` = `slice_begin` + one full-budget runner call +
  `slice_end`), so both fronts share the exact slice semantics.
- Consequences (all BINDING):
  - Slice semantics are unchanged: one `slice_begin` (mark executing + apply queued signals), one
    total reduction budget, **one** requeue at the process's own priority, one `finish_slice`
    in-flight decrement. `run_one`/`slice_end` remain the only place a requeue/terminate happens.
  - Independent processes, cross-thread `deliver_simple` calls, timer fires and dirty completions can
    be serviced **inside** another process's slice (that is the point of the change) — but a process
    still does not *run* in parallel with another: exactly one thread is inside the table at a time.
  - A runner that exhausts the budget it is handed (the real engine) behaves exactly as before. A
    runner that returns `Yield` **before** spending its quantum is continued with the next quantum of
    the same slice instead of being requeued (the pool cannot observe how much of a quantum a runner
    consumed); `tests/smp.rs`'s progress tests pin that (`YIELDS + 1` runner invocations, **one**
    recorded slice per process). `lock_quantum >= reductions_per_slice` reproduces the old
    one-lock-hold-per-slice behavior exactly.
  - Between quanta a peer may **terminate** the process (link exit / `exit/2`), so every quantum
    re-checks `table.contains(pid)` and stops the slice when it is gone. No other scheduler can pick
    the pid up: it is in no queue and not `Runnable`, which is what every wake-sweep guard requires.

## Locking discipline (BINDING — follow it in every change)
Three lock families: the **queue locks** (`Mutex<RunQueue>` inside `Queues`), the **table lock**
(`SharedProcessTable`) and the **signals mutex** (`PoolSignals`). The rules:
1. **Never hold a queue lock while acquiring the table lock**, and never hold the table lock while
   acquiring a queue lock. Allowed orders: *queue → release → table* and *table → (buffer) → release
   → queue*.
2. [`run_locked_quanta`]/`drain_woken_into` invoke their requeue closure **while the table lock is
   held**, so a worker must **buffer** requeues into a local `Vec` and only touch the local queue
   lock after the guard drops. The timer/dirty sinks do the same.
3. The **only** nested acquisition is inside `PoolSignals::park`/`wait_for_idle`, which hold the
   *signals* mutex and then check a queue: order *signals → queue*. The reverse never happens (a
   notifier releases its queue lock before signalling), so the hierarchy cannot deadlock.
4. In-flight accounting is done *inside* the critical section that pops a pid (and stays incremented
   across the quanta of a slice), so a concurrent idle check can never observe a pid that is neither
   queued nor in-flight.
5. `worker.rs` flushes `buf` **before** `finish_slice` (which decrements in-flight), otherwise a
   yielded pid could be invisible to `is_idle()` for a moment.

### The `Send`/`Sync` invariant (BINDING)
`SharedProcessTable` carries two `unsafe impl Send`/`Sync`. They are **load-bearing**: `parking_lot::Mutex<T>`
is `Send + Sync` only when `T: Send`, and the pool shares the table as `Arc<SharedProcessTable>`, so both
bounds are required (removing them breaks compilation and the `shared_table_is_send_and_sync` unit test in
`mod.rs`). They are asserted on the **wrapper only** — never on bare `ProcessTable`.

`ProcessTable` is `!Send` for **five** reasons (each reproduced, with the soundness argument, in the
`// SAFETY:` comments on the impls in `mod.rs`):
1. `beam_alloc::Heap`'s `base: *mut Eterm` (`beam_alloc/src/heap.rs`), reached via `Process.collector →
   beam_gc::Collector → HeapSpace → beam_alloc::Heap`.
2. `beam_gc::MsoHandle`'s `ptr: NonNull<Mso>` (`beam_gc/src/mso.rs`), reached via
   `beam_message::OffHeapTable`.
3. `Dict.atoms: Rc<AtomTable>` (`dict.rs`) — `Rc`'s refcount is not atomic, so `Rc` is `!Send`.
4. `ProcessTable.router: Option<Box<dyn DistSignalRouter>>` (`ptab.rs`) — the trait object has no `Send`
   bound.
5. `ProcessTable.exit_observer: Option<Box<dyn ProcessExitObserver>>` (`ptab.rs`) — likewise no `Send`
   bound.

Soundness rests on the **single-lock invariant**: `SharedProcessTable` exposes the table ONLY through
`lock`/`try_lock`, which return a `MutexGuard` that is itself `!Send`, so at most one thread is ever inside
the table and no `!Send` value (nor any `&mut ProcessTable`) can move off the locking thread. Under that
lock the `beam_alloc::Heap` uniquely owns a `Send` carrier block whose address is stable across the move,
the `MsoHandle` uniquely owns an immutable `Box<dyn Any + Send>` payload, and the two trait objects are
invoked only while the lock is held and carry no thread-affine state. **The `Rc<AtomTable>` needs one
explicit rule:** every process's `Rc<AtomTable>` must come from `Dict::new()` (production does —
`process.rs`) and must **never** be cloned or shared outside the table — do NOT clone a dictionary's
`Rc<AtomTable>` to another thread, and do NOT adopt `Dict::with_atoms` (test-only today) with an `Rc` the
host also holds on another thread.

Durable fix — a **documented handoff** to the upstream crates, do **not** make it here: `unsafe impl Send
for beam_alloc::Heap` and for `beam_gc::MsoHandle`; `Dict.atoms: Rc<AtomTable>` → `Arc<AtomTable>`
(`AtomTable` is already `Send + Sync`); `trait DistSignalRouter: Send` and `trait ProcessExitObserver:
Send`. After those, the two impls (and this section) can be deleted. (`beam_alloc::RefcBinary` already
carries `unsafe impl Send`, so it is **not** a blocker.)

## Design Decisions
- **The SMP pool reuses the frozen slice.** Concurrency is added *around* the slice, never inside it:
  the worker calls the same `pub(crate)` helpers the single-threaded `Scheduler` uses, and the only
  difference is the requeue closure (it buffers instead of touching a queue). Do **not**
  re-implement slice logic in a worker.
- **The bounded-quantum lock instead of one lock per slice.** See above: the durable narrowing that
  does not need a `ProcessRunner` seam change. Truly parallel execution needs a per-process (per-PCB)
  lock and a slice API that hands the runner its own process plus a *re-entrant* table handle — i.e.
  a `ProcessRunner::run(&mut self, pid, &mut SliceContext<'_>, budget)` seam change owned downstream
  (`beam_rt`) plus per-slot locking in `ptab.rs`. That is deliberately NOT done here.
- **`SharedProcessTable`'s `unsafe impl Send/Sync` is a contained handoff.** The five `!Send` causes,
  the single-lock soundness argument, the `Rc<AtomTable>` never-shared rule and the durable-fix handoff
  live in the `Send`/`Sync` invariant under *Locking discipline* above (and in the `// SAFETY:` comments
  in `mod.rs`). Do not widen this `unsafe`.
- **Non-flaky `wait_idle` via an in-flight counter**, and **loss-free park/notify** (the predicate is
  re-checked under the signals mutex immediately before `Condvar::wait`; every notifier takes the
  same mutex before signalling).

## Known Issues (intentionally NOT covered)
- **No intra-process parallelism.** The lock is narrower (one quantum) but still exclusive: across
  all schedulers exactly one process executes at a time. Lock-free/threaded run queues, scheduler
  balancing and the `+S`/`+SDcpu`/`+SDio` flag parsing are not modelled. Port tasks are not
  load-balanced across schedulers.
- **`unsafe impl Send/Sync for SharedProcessTable`** is still required (see the `Send`/`Sync` invariant
  under *Locking discipline* for the exact five-cause list and the durable-fix handoff).
  `ThreadProgress` implements the barrier but has **no** delayed-dealloc coupling with `beam_alloc`.
- **Dirty-job term plumbing** is absent (a `DirtyJob` carries no live heap data) and a timer message
  must be **thread-valid** (an immediate or literal-area term — no heap-resident terms).
- **A parked worker only re-checks its own queue and the injector** (`has_takeable_work` excludes
  peers, so a lone pid on a peer's queue never wakes a parked scheduler). Stealing is therefore
  opportunistic (it happens when a worker is awake and finds its own queue + injector empty).

## Test Strategy
- In-module unit tests: `mod.rs` (config defaults/setters incl. `lock_quantum`, the shared-table
  lock/try_lock/send-sync assertions), `queues.rs` (own → injector → steal order, in-flight
  accounting, idle predicates), `worker.rs` (park/notify, `wait_for_idle`, first-error retention),
  `pool.rs` (start/run/drain smoke, 0-scheduler clamp, safepoint on a parked pool).
- Integration evidence (crate `tests/`):
  - `tests/smp.rs` (11 tests) — N-on-M-scheduler progress, work stealing, a timer/dirty/send wake,
    lifecycle/observables; the progress tests pin "`YIELDS + 1` runner invocations per process, one
    recorded slice each" (i.e. a process's early yields are quanta of one slice).
  - `tests/lock_scope.rs` (3 tests) — the narrowed lock's evidence: `a_slice_is_serviced_as_bounded_quanta`
    (structural: one slice = `ceil(budget/quantum)` runner invocations; `quantum == budget`
    reproduces the coarse behavior), `an_independent_process_and_a_cross_thread_send_progress_inside_a_slice`
    (a cross-thread send is serviced within one quantum and the woken peer runs *inside* the other
    process's slice — both assertions fail under a whole-slice lock), and
    `per_sender_message_and_exit_order_is_identical_on_one_and_four_schedulers` (determinism of
    per-sender message order + `{'EXIT', …}` placement across 1 and 4 schedulers).
  - `tests/timers.rs`, `tests/dirty.rs`, `tests/progress.rs` — the timer service, the dirty pools and
    the progress barrier.
- **Anti-flakiness.** Every assertion is forced by **synchronization**, never a bare
  `std::thread::sleep`: `mpsc` channels + `recv_timeout`, `std::sync::Barrier`/gates, and
  `SchedulerPool::wait_idle(Some(timeout))` (which also requires the in-flight count to be zero, so
  counters read after it are exact). `tests/lock_scope.rs` is the only place with wall-clock bounds:
  they are deliberately loose (a send must be serviced far below the slice duration, and the slice
  must last far above that bound). A wedged pool fails with a clear timeout instead of hanging.
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_process`.

## Routing Table
- `mod.rs` → SMP module docs (topology, locking discipline, the lock-quantum rule, `unsafe`/`Send`), `SchedulerConfig`, `SharedProcessTable`
- `queues.rs` → `Queues` (injector + per-scheduler locals + `InFlight`; `Acquired`; own→injector→steal; length/idle predicates)
- `worker.rs` → `Worker` (the scheduler-thread loop), `run_locked_quanta` (the bounded-quantum slice), `PoolSignals` (park/notify + idle wait), `record_error`
- `stats.rs` → `SchedulerStats`, `SchedulerStat` + the internal `WorkerCounters`/`InFlight` atomics
- `pool.rs` → `SchedulerPool` (start/spawn/enqueue/deliver/timer/dirty/stats/idle/safepoint/shutdown/join + the `TimerSink`/`DirtyCompletionSink` impls)
- `../scheduler.rs` → the frozen slice halves (`run_one`, `slice_begin`, `slice_end`, `drain_woken_into`) and `DEFAULT_LOCK_QUANTUM`/`DEFAULT_REDUCTIONS_PER_SLICE`
- `../timer.rs`, `../dirty.rs`, `../progress.rs` → the timer service, the dirty pools and the barrier this module owns
- `../../tests/smp.rs`, `../../tests/lock_scope.rs`, `../../tests/timers.rs`, `../../tests/dirty.rs`, `../../tests/progress.rs` → the integration evidence for this module
- `../../../../erts/emulator/beam/erl_process.c` → scheduler/run-queue ground truth (read-only)
