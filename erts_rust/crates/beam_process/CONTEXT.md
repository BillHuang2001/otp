# `beam_process` — Processes, Scheduler, Run Queues, Signals
## Intent
Owns the **OS-thread-level concurrency plumbing** of the Rust ERTS rewrite (Stage 5, Part II §H): the
per-process control block (PCB), the four-level run queue, the cooperative scheduler loop, the
**multi-scheduler (SMP) pool** (`SchedulerPool`: N scheduler threads over one shared table,
per-scheduler run queues with work stealing, the cross-scheduler `ThreadProgress` barrier, the
high-level `TimerService` and the two `DirtyPool`s), the **exit-signal / link / monitor** machinery
(the decoupled per-process signal queue plus its signal-message builders), the process table (`ptab`)
with the local name registry, the **port-id table with its ownership/link/monitor surface**, and the
local records for **remote (distributed) links/monitors**. It is what makes Erlang **processes** real:
isolation, `spawn`, links, monitors and exit propagation — the process half of workspace
`../CONTEXT.md` §1 **A.1** (process isolation / link / monitor / exit semantics).
It is consumed by `beam_bifs` (spawn/send/receive/`erlang:process_info`/`monitor`/`link` BIFs),
`beam_io` (ports + the process side of I/O completion) and `beam_rt` (boot/CLI/`system_info`).
It **drives** `beam_gc` (collector + MSO) at safepoints and **uses** `beam_message` (`Queue` mailbox
+ `send`/`receive` copy engine) — it reimplements neither (Prevent-Duplication).
Legacy C ground truth (read-only): `../../erts/emulator/beam/erl_process.{c,h}` (PCB, scheduler, run
queues), `erl_proc_sig_queue.c` (signal queue), `erl_monitor_link.c` (links/monitors),
`erl_ptab.{c,h}` (process/port table + name registry), `erl_process_dict.c`, `erl_vm.h`
(`MAX_REG`/`MAX_ARG`), `erl_thr_progress.c` (thread-progress barrier), `erl_hl_timer.c` (timer wheel)
and `erl_nfunc_sched.c` (the NIF→dirty-scheduler trampoline).
## Milestone-1 scope
The crate has **two scheduler fronts**: the single-threaded `Scheduler` loop (whose slice semantics are
frozen) and the SMP `SchedulerPool` that runs `N` scheduler threads over one shared table. Both execute
the **same** slice logic — the pool reuses it through the shared `run_one`/`drain_woken_into` helpers,
so the SMP front adds *concurrency*, not a second scheduler implementation. "Correct" means the pid
table, the **four-level** run-queue scheduling order (max/high/normal/low), message delivery, the
**decoupled per-process signal queue** (exit signals + monitor-DOWNs, applied at schedule-in or eagerly
for a non-running receiver), the process dictionary and saved-argument sub-rootsets, the **port
ownership/link/monitor** surface, **remote (distributed) links/monitors** routed through the
`DistSignalRouter` seam, and a safepoint GC over the process root set all behave per the C semantics
for the common cases, and are covered by tests.
Still **not** modelled: lock-free/threaded run queues, scheduler balancing and `+S`/`+SDcpu`/`+SDio`
flag parsing, per-process **parallel** execution (the pool holds the table lock for a whole slice, so
exactly one process runs at a time across all schedulers), dirty-job term plumbing (a `DirtyJob`
carries no live heap data), and heap-resident timer messages (a timer message must be thread-valid —
an immediate or a literal-area term).
## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_process::prelude::*`. Each module's **full** method list
lives in its `src/*.rs` top-of-file `//!` rustdoc; this section names the frozen seam types and the
signatures dependents program against.
### `pid.rs` — process/port identity (bit-exact with `beam_term::immediate`)
- `ProcessId { number: u64, serial: u64 }` (`Copy + Eq + Hash + Ord + Debug`): `new`, `number`,
  `serial`, `to_eterm() -> Eterm` (= `make_internal_pid(make_pid_data(serial, number))`),
  `from_eterm(Eterm) -> Result<ProcessId>` (`BadArg` unless an internal pid), `is_process(Eterm)`.
- `PortId { number: u64 }` — same shape via `make_internal_port`/`is_internal_port`.
- `MAX_PROCESS_NUMBER = (1 << 28) - 1` (`PID_NUM_SIZE`); `MAX_PROCESS_SERIAL` = `2^32 - 1` (`PID_SER_SIZE`).
### `registers.rs` — the root-set containers (x-register file + Erlang stack)
Honors the crate-layer root-set contract: `x_reg_array[0..Live)` **plus the whole stack**, no frame
liveness maps.
- `MAX_REG = 1024` (`erl_vm.h`), `MAX_ARG = 255`.
- `XRegs` (boxed `[Eterm; MAX_REG]` + `live`): `new` (all `NIL`, `live == 0`), `live()`,
  `set_live(n) -> Result<()>`, `get(i)`/`set(i, v)`, `live_slice()`/`live_slice_mut()`.
- `Stack` (`Vec<Eterm>`): `push`/`pop`, `as_slice`/`as_mut_slice`, `clear`, `truncate`.
- `Registers { x, stack }`: `x()/x_mut()`, `stack()/stack_mut()`, plus the GC pair
  `gather_roots(&self, out: &mut Vec<Eterm>) -> usize` (appends `x[0..live)` then every stack word;
  returns the `x_live` split point) / `scatter_roots(&mut self, roots, x_live)`.
### `process.rs` — the process control block (PCB)
- `ProcessState { Runnable, Running, Waiting, Suspended, Exiting, Terminated }` (`Copy`).
- `DeliverOutcome { Enqueued, Woke }` (`Copy + Eq`) — `Enqueued` = recipient was not blocked; `Woke` =
  recipient was blocked (`Waiting`/`Suspended`) and is now `Runnable`; `woke() -> bool`.
- `ProcessFlags` — a `u32` bitset (no `bitflags` dep): `trap_exit`, `system`, `in_run_queue`,
  `garbage_collecting`, `active`, `sensitive`; per-flag get/set + `bits()/from_bits()`.
- `Process` — the PCB, owning `pid`, `state`, `flags`, `priority`, `save_calls: u16`,
  `collector: beam_gc::Collector` (young + mature heap spaces), `off_heap: beam_message::OffHeapTable`
  (MSO handles), `mailbox: beam_message::Queue`, `regs: Registers`, `dict: Dict`, `saved_args:
  SavedArgs`, `signals: SignalQueue`, `reductions: u64`, `links: Links`, `monitors: Monitors`
  (outgoing), `monitored_by: Vec<MonitoredBy>` (incoming), `exit_reason: Option<Eterm>`.
  - `new(pid) -> Result<Process>` / `with_policy(pid, policy)`.
  - `wake(&mut self) -> bool` — `Waiting`/`Suspended` ⇒ `Runnable` + `true` (the blocked-recipient
    wake used by resume-on-send).
  - `heap()/heap_mut()` (the **young** mutator heap, `collector.young_mut().heap_mut()`); the
    `priority`/`save_calls`/`sensitive`/`trap_exit` accessors; `mailbox(_mut)`, `registers(_mut)`,
    `dict(_mut)`, `saved_args(_mut)`, `signals(_mut)`/`pending_signals()`, `off_heap(_mut)`,
    `links(_mut)`, `monitors(_mut)`, `monitored_by(_mut)`, `exit_reason`/`set_exit_reason`.
  - **message delivery:** `deliver(from: Eterm, term: Eterm, src_off_heap: &OffHeapTable) ->
    Result<DeliverOutcome>` — copies `term` into the young heap (collect-and-retry once on
    `AllocKind::Heap`/`Message` exhaustion), enqueues the `Message`, then `wake()`s;
    `deliver_simple(from, term)` — the no-off-heap form; `receive(pred) -> Option<Message>` (delegates
    to `beam_message::receive`).
  - **GC:** `collector()`, `should_collect(need_words)`, `heap_used_words()`, `collect_if_needed(need)`,
    and `collect() -> Result<Collection>` (gather root set → run collector → scatter back; see Design
    Decisions).
### `dict.rs` — the per-process dictionary
- `Dict` (`Default`) backing `erlang:put/2`/`get/1`/`erase/1`: `put(Key, Val) -> Option<Eterm>` (last
  put wins, returns the replaced value), `get`, `erase`, `erase_all`, `get_keys`,
  `get_keys_for_value`, `entries()`.
  - **GC pair:** `gather_roots(out) -> usize` (appends each entry's key then value) /
    `scatter_roots(roots, start)` (never-panic).
  - **Semantics:** `None` is the C `THE_NON_VALUE` (absent; the BIF layer maps it to `undefined`).
    Term-agnostic (no `AtomTable`): keys compare by raw `Eterm` word equality. Holds terms resident in
    THIS process's young heap (a `Process::collect` sub-rootset).
### `fragments.rs` — saved BIF/NIF arguments
- `SavedArgs` (`Default`) — the saved BIF/NIF argument root list (the C `ErlSaveCallArgs`):
  `push`/`pop`, `as_slice`/`as_mut_slice`, `len`/`clear`, plus the GC pair `gather_roots`/`scatter_roots`
  (the region of `Process::collect`'s root set after the dictionary, before the pending signals).
### `run_queue.rs` — the ready queue (four priority levels)
- `Priority { Max=0, High=1, Normal=2, Low=3 }` (`Copy + Ord + Hash`; `Default = Normal`). The derived
  `Ord` is the C *numeric* order (`Max < High < Normal < Low`), which is **NOT** scheduling precedence
  — precedence is `LEVELS`. `LEVELS = [Max, High, Normal, Low]` (highest-first); `index()/from_index(u8)`
  (the C `PRIORITY_*` value); `name()/from_name(&str)`.
- `RunQueue` — **four per-priority FIFOs**; `dequeue` serves the highest-priority non-empty level, FIFO
  within a level. `enqueue(pid)` ⇒ **normal**; `enqueue_with_priority(pid, Priority)`;
  `dequeue`/`dequeue_with_priority`; `len`/`len_at`; `contains`/`priority_of`/`remove`;
  `iter`/`iter_at`; `clear`.
  - `split_off_half(&mut self) -> Vec<ProcessId>` — removes up to half the queue's pids (highest
    priority first, in `dequeue` order) so a work-stealing victim keeps at least half (0/1 pids ⇒
    empty). The victim half of the SMP steal.
  - **Fairness deviation (documented):** the C keeps MAX/HIGH dedicated but lets **NORMAL and LOW share
    one physical queue**, deferring LOW by `schedule_count` (`RESCHEDULE_LOW = 8`, `erl_process.h`).
    This crate uses **four physically distinct FIFOs**, so LOW runs only once MAX/HIGH/NORMAL are empty;
    the relative precedence is unchanged (LOW last), only the LOW/NORMAL interleaving ratio differs.
### `scheduler.rs` — the single-threaded scheduler loop + the engine seam
- `RunOutcome { Yield, Wait, Suspend, Exit(Eterm) }` — what happened in a reduction slice
  (`Exit(reason_term)` = the process terminated with that reason).
- `ProcessRunner` — **the engine seam** (`beam_engine` is NOT a dependency; `beam_rt`/`beam_engine`
  implement it over an `ExecutionContext`): `fn run(&mut self, pid: ProcessId, table: &mut
  ProcessTable, budget: u32) -> Result<RunOutcome>`.
- `Scheduler` — `new()` (default `DEFAULT_REDUCTIONS_PER_SLICE = 4000`), `with_slice(u32)`,
  `enqueue(pid)` (⇒ normal) / `enqueue_with_priority`, `progress()/progress_mut()`, and:
  - `run_slice<R: ProcessRunner>(&mut self, table, runner) -> Result<Option<ProcessId>>` — pop one pid,
    **service its pending signals** (schedule-in; a fatal signal can terminate the pid, in which case
    `run_slice` returns early having run zero reductions), run one slice; `Yield` ⇒ requeue at the tail
    of the process's **own** current priority; `Wait`/`Suspend` ⇒ leave the queue (state set by the
    runner); `Exit(reason)` ⇒ `table.terminate(pid, reason)`. After the outcome (and on the empty-queue
    early return) it absorbs the table's wake list via `resume_woken`.
  - `resume_woken(&mut self, table) -> usize` — drain `table.drain_woken()`; enqueue each still-live
    `Runnable` pid not already in a run queue **at its own priority**; returns the count. Also the hook
    `beam_rt` calls after a send made **outside** a slice.
  - `run_ready<R>(&mut self, table, runner, max_slices: Option<u64>) -> Result<u64>` — run until the
    ready queue is empty or `max_slices` slices ran.
- **Reduction accounting is delegated to the runner.** `DEFAULT_REDUCTIONS_PER_SLICE` is handed to
  `ProcessRunner::run` as its `budget` argument; the scheduler never reads or resets
  `Process::reductions` (`process.rs:367-379` — the accessors are exercised only by tests). The runner
  owns the reds count and decides when to return `RunOutcome::Yield`.
- **Shared slice helpers (the SMP reuse seam).** `pub(crate) run_one<R, F>(pid, table, runner,
  reductions, enqueue: F)` and `pub(crate) drain_woken_into<F>(table, enqueue: F)` hold the slice
  semantics (`run_slice`/`resume_woken` delegate to them). The SMP pool calls the same helpers with a
  closure that **buffers** requeues instead of touching a queue, so both scheduler fronts run identical
  slice logic.
- **The single-threaded orchestration contract `beam_rt` follows** (`Scheduler::resume_woken` is the
  only coupling): spawn ⇒ `table.spawn_default()?` then `scheduler.enqueue(pid)`; a runner that blocks
  on an empty receive sets `ProcessState::Waiting` itself **before** returning `RunOutcome::Wait`; a
  send from **inside** a runner ⇒ `table.deliver(_simple)(…)` (absorbed at slice end); a send from
  **outside** a slice ⇒ `table.deliver(_simple)(…)` then `scheduler.resume_woken(&mut table)`; run ⇒
  `scheduler.run_ready(&mut table, &mut runner, None)`.
### `smp/` — the multi-scheduler (SMP) pool
The `Scheduler` counterpart for SMP: `N` scheduler threads over one shared process table. Re-exported at
the crate root (`beam_process::{SchedulerPool, SchedulerConfig, SchedulerStats, SchedulerStat}`). Five
files:
- `smp/mod.rs` — module docs (**incl. the BINDING locking discipline**) plus:
  - `SchedulerConfig` (`Copy + Eq + Default`) — `new()` defaults: **1** scheduler, 0 dirty workers,
    timers on, `DEFAULT_REDUCTIONS_PER_SLICE`; chainable setters
    `set_schedulers`/`set_reductions_per_slice`/`set_dirty_cpu`/`set_dirty_io`/`set_timers` (+ the
    matching getters), `with_schedulers(n)`.
  - `SharedProcessTable { inner: Mutex<ProcessTable> }` — the table behind one `parking_lot::Mutex`;
    `new(ProcessTable) -> Arc<Self>`, `lock()`, `try_lock()`. Carries two **documented `unsafe impl
    Send`/`unsafe impl Sync`** (a contained handoff, see Design Decisions).
- `smp/queues.rs` — `pub(crate) Queues`: a shared **injector** `Mutex<RunQueue>` + one `Mutex<RunQueue>`
  per scheduler index + the shared `InFlight` counter; `acquire(index)` tries own queue → injector →
  round-robin **steal of half** a peer's queue (`RunQueue::split_off_half`), returning
  `Acquired::{Pid, Stolen, Idle}`; length queries (`injector_len`/`local_len`/`local_lengths`/
  `total_len`) and the idle predicates `has_takeable_work(index)`/`is_idle()` (`total_len() == 0 &&
  inflight == 0`).
- `smp/worker.rs` — `Worker::run` (the scheduler-thread loop): update thread progress, acquire work
  (own → injector → steal), run one slice via the shared `run_one`/`drain_woken_into` helpers with
  requeues **buffered** in a worker-local `buf`, and flush `buf` to the local queue only after the
  table guard drops. Plus `pub(crate) PoolSignals` (work/idle condvars + a stop flag; the park
  predicate is re-checked under the signals mutex so a wakeup cannot be lost) and `record_error`.
- `smp/stats.rs` — `SchedulerStats { schedulers, run_queue_len, slices, steals, active,
  dirty_cpu_pending, dirty_io_pending, timers_pending }` and `SchedulerStat { index, run_queue_len,
  slices, steals, active }` + the internal `WorkerCounters`/`InFlight` atomics.
- `smp/pool.rs` — `SchedulerPool<R: ProcessRunner + Send + 'static>`: `start(config, table,
  make_runner) -> Result<Self>`; `schedulers`, `progress`, `table`, `timer_service`, `dirty_pool`,
  `spawn_default`, `spawn(policy)`, `enqueue`/`enqueue_with_priority`, `deliver_simple`, `send_after`,
  `submit_dirty`, `stats`, `scheduler_stats`, `run_queue_len`, `run_queue_lengths`,
  `wait_idle(Option<Duration>)`, `safepoint`, `take_error`, `shutdown`, `join`; plus the
  `TimerSink`/`DirtyCompletionSink` impls. **Lifecycle:** `schedulers() == 0` is **clamped to 1**;
  `shutdown()` abandons any still-queued pids (the processes stay in the table); `join(self)` = stop +
  join scheduler threads + timer-service shutdown/join + `Arc::try_unwrap` the dirty pool and join it
  (falling back to `shutdown()` when a caller still holds a clone).
### `timer.rs` — the high-level timer service
- `TimerService` — one background thread over a deadline min-heap (the `erlang:send_after/3`,
  `start_timer/3` and `receive ... after T` plumbing): `new()`, `start(Arc<dyn TimerSink>)`
  (idempotent), `send_after(Duration, ProcessId, Eterm) -> TimerRef`, `cancel(TimerRef) -> bool`,
  `pending()`, `next_deadline() -> Option<Instant>`, `is_running()`, `shutdown()` (terminal), `join()`.
- `TimerRef { id, generation }` (`Copy`) with `id()`/`generation()`.
- `TimerSink: Send + Sync { fn timer_fired(&self, target: ProcessId, message: Eterm); }` — the seam
  the pool implements.
- **Message contract:** the stored message must be **thread-valid** — an immediate (small int/atom/pid)
  or a literal-area/interned term; a term resident in a per-process heap is a caller contract violation
  (needs an off-heap handle; documented follow-up).
### `dirty.rs` — the two dirty scheduler pools
- `DirtyKind { Cpu, Io }` (`ALL`, `name()` = `"dirty_cpu_scheduler"`/`"dirty_io_scheduler"`).
- `DirtyOutcome { Done, Reschedule(ProcessId), Deliver { target, message } }` — routing data the normal
  scheduler acts on (`Reschedule` re-enters the pid; `Deliver` delivers and wakes).
- `DirtyJob = Box<dyn FnOnce() -> DirtyOutcome + Send + 'static>` — a plain closure carrying **no**
  live heap data. `DirtyCompletionSink: Send + Sync { fn dirty_completed(&self, DirtyOutcome); }` —
  implemented by the pool.
- `DirtyPool` — `start(cpu, io)`, `set_sink(Arc<dyn DirtyCompletionSink>)`,
  `cpu_schedulers()`/`io_schedulers()`/`schedulers(kind)`, `submit(kind, job)`, `pending(kind)`,
  `shutdown()`, `join()`. A kind with **0 workers buffers** jobs (never drops on submit, discards at
  join); a panicking job does not kill the pool.
### `progress.rs` — thread progress (the cross-scheduler barrier)
- `ThreadProgress` — the real barrier (cheap to clone; every clone shares the generation,
  managed-thread registry and blocking counter): `new()`, `generation()`, `bump() -> u64`, `wait(u64)`
  (blocks until every **active** managed thread confirmed the generation), `safepoint() -> u64`
  (`bump`+`wait`), `managed_thread() -> ManagedThread`, `managed_count()`, `set_waker(Arc<dyn Fn() +
  Send + Sync>)`, `is_blocking()`/`block()`/`unblock()`/`blocking_count()`.
- `ManagedThread` — the per-thread slot a scheduler registers: `progress()`, `update()` (confirm the
  current generation), `is_active()`/`set_active(bool)` (an inactive/parked thread is already at a
  safepoint and is excluded from the wait), `id()`.
### `signal.rs` — exit signals, reasons and signal-message builders
- `SignalAtoms { exit, down, process, port, normal, kill, killed }` — the well-known atom indices used
  to build signal messages; `from_table(&mut AtomTable)` interns `'EXIT'`/`'DOWN'`/`process`/`port`/
  `normal`/`kill`/`killed`; accessors incl. `port()`, `normal_term()/kill_term()/killed_term() ->
  Eterm`.
- `ReasonClass { Normal, Kill, Other }` + `SignalAtoms::classify(reason) -> ReasonClass`.
- `ExitSignal { from: ProcessId, reason: Eterm }` (`Copy`).
- `ExitPolicy { Terminate(Eterm), Trap, Ignore }` — `Trap` = deliver `{'EXIT', From, Reason}` as a
  message.
- `exit_policy(reason, trapping, &SignalAtoms) -> ExitPolicy` — the propagation rule: `kill` ⇒
  `Terminate(killed)` (untrappable); else trapping ⇒ `Trap`; else `normal` ⇒ `Ignore`; else
  `Terminate(reason)`.
- **Signal-message builders** (each allocates on the *receiving* process's heap): `build_exit_message
  (heap, atoms, from: ProcessId, reason)` (`{'EXIT', From, Reason}`); `build_exit_message_term(heap,
  atoms, from: Eterm, reason)` (the general form, whose `from` may be an external pid term);
  `build_down_message(heap, atoms, ref, pid, reason)` (`{'DOWN', Ref, process, Pid, Reason}`);
  `build_down_message_port(...)` (the `port`-tagged form); `build_down_message_remote(...)`
  (materialises the external-pid target on the same heap); and the fully general
  `build_down_message_term(heap, atoms, ref, tag: Atom, target: Eterm, reason)`.
### `signal_queue.rs` — the decoupled per-process pending-signal queue
A signal is *queued* on the receiver and applied later (see the eager-vs-queued rule under `ptab.rs`);
its `reason` is copied into the receiver's own heap at enqueue time, so the queue is an in-heap
sub-rootset.
- `PendingSignalKind { Exit, Down }`.
- `PendingSignal { Exit { from, reason }, Down { reference: MonitorRef, target: ProcessId, kind:
  MonitorKind, reason } }` (`Copy`) — the queued signal; `exit(..)`/`down(..)` constructors, `kind()`,
  `reason()`.
- `SignalQueue { entries: VecDeque<PendingSignal> }` (`Default`) — the per-process FIFO:
  `push`/`pop`/`peek`/`drain`/`clear`/`iter` (application order), plus the GC pair
  `gather_roots(out) -> usize` (one word per queued signal's `reason`) / `scatter_roots(roots, start)`
  (never-panic). Monitor references are stored by value, not on the heap.
- **Ordering:** a total FIFO order per receiver (a single FIFO) — which subsumes the C's
  per-`(sender, receiver)` order; **not** cross-ordered with the mailbox.
### `link.rs` — bidirectional links
- `Links { pids: Vec<ProcessId> }` — `add(pid) -> bool` (false if already linked), `remove(pid) ->
  bool`, `contains(pid)`, `iter()`, `drain()`.
### `monitor.rs` — monitors and monitor references
- `MonitorRef { id: u64 }` (`Copy + Eq + Hash`) — `id()`, `to_eterm(self, heap) -> Result<Eterm>`
  (builds the on-heap `Ref` thing, see Design Decisions).
- `MonitorKind { Process, Port }`.
- `Monitor { reference, target: ProcessId, kind }` (outgoing) / `MonitoredBy { reference, watcher:
  ProcessId, kind }` (incoming).
- `Monitors { entries: Vec<Monitor> }` — `push`, `remove(reference) -> Option<Monitor>`,
  `find(reference)`, `iter`, `drain`.
- `RefAllocator { next: u64 }` — `next() -> MonitorRef` (monotonic, never reuses an id). Owned by
  `ProcessTable`.
### `ptab.rs` — the process/port table + name registry + signal/link/monitor hub
The single place that mutates ≥2 processes (mirrors the C ptab lock domain); M1 single-threaded, so it
holds `Process` values directly. (`PortTable` lives in `port.rs`.)
- `ProcessTable { slots, free, names: HashMap<Atom, ProcessId>, refs: RefAllocator, atoms: SignalAtoms,
  ready: Vec<ProcessId>, remote_links, remote_monitors, router: Option<Box<dyn DistSignalRouter>> }`
  (`Slot { serial: u64, process: Option<Process> }`). `ready` is the **wake list** (a delivery/signal
  pushes a woken pid; `drain_woken` returns **and** clears it); `router` is the distribution seam
  (`None` ⇒ record-only).
  - **allocation / lookup:** `new(atoms)`, `alloc_pid() -> Result<ProcessId>` (reuses a free slot,
    bumping its serial; `SystemLimit` on exhaustion), `spawn(policy)`/`spawn_default()`,
    `insert(Process)`, `get(pid)`/`get_mut(pid)`, `take(pid)` (bumps the slot serial **and drops the
    pid from the wake list**), `remove`, `contains`, `len`, `pids()`.
  - **priority:** `set_priority(pid, Priority) -> Result<()>` (unknown pid ⇒ `TermNotFound`).
  - **message delivery:** `deliver(to, from, term, src_off_heap: &OffHeapTable) ->
    Result<DeliverOutcome>` (calls the target's `Process::deliver`; on `Woke` pushes `to` onto the wake
    list; unknown `to` ⇒ `TermNotFound`), `deliver_simple(to, from, term)`, `drain_woken() ->
    Vec<ProcessId>`, `woken() -> &[ProcessId]`.
  - **signals / links / monitors (local pids):** `apply_exit_signal(target, ExitSignal) ->
    Result<ExitEffect>`, `link(a, b)`, `unlink(a, b)`, `send_exit(from, to, reason)` (the `exit/2` op),
    `monitor(watcher, target, kind) -> Result<MonitorRef>`, `demonitor(watcher, reference) ->
    Result<bool>`, `terminate(pid, reason)`.
  - **decoupled signal servicing:** `service_signals(pid) -> Result<()>` — drains + applies the pid's
    pending-signal queue in FIFO order, cascading to any process those signals affect (the scheduler
    calls it at schedule-in); `pending_signals(pid)`, `signals(pid) -> Result<&SignalQueue>`.
  - **arbitrary pid terms:** `link_term`/`unlink_term(a, b: Eterm)`, `monitor_term(watcher, target:
    Eterm, kind) -> Result<MonitorRef>`, `send_exit_term(from, to: Eterm, reason)` — a local pid routes
    to the local op, an external pid is recorded + routed (see `remote.rs`); `remote_links(pid)`/
    `remote_monitors(pid)`.
  - **distribution seam:** `set_dist_router(Box<dyn DistSignalRouter>)`, `take_dist_router()`,
    `has_dist_router() -> bool`.
  - **whole-node down / port close:** `node_down(node: Eterm, reason)` — resolves + purges every remote
    relationship with `node` (a `{'DOWN', …}` per monitor, a trapped/fatal `{'EXIT', …}` per link);
    `port_closed(&mut PortTable, port, reason)` — drains the port's owner/links/monitors and delivers
    the `{'DOWN', Ref, port, Port, Reason}` / `{'EXIT', Port, Reason}` shapes.
  - **name registry:** `register(name, pid)`, `unregister(name)`, `whereis(name) -> Option<ProcessId>`,
    `registered()`.
- `ExitEffect { Terminated, Trapped, Ignored }` — the outcome of `apply_exit_signal` (whether the
  target was terminated, got a `{'EXIT'}` message, or ignored the signal).
- **Eager-vs-queued rule (the signal ordering contract).** A signalling operation **enqueues** a
  `PendingSignal` on the receiver (copying the reason into the receiver's own heap) — never applies it
  inline. Servicing then happens (a) at the receiver's **schedule-in** (`service_signals`, called by
  `run_slice`) and (b) **eagerly at the end of the sending operation** for a receiver that is *not* the
  currently `Running` process (ERTS's early signal handling): `apply_exit_signal`/`terminate`/
  `send_exit` share one rule — every receiver is added to the service worklist unless it is
  `ProcessState::Running`. `apply_exit_signal` returns the `exit_policy`-predicted `ExitEffect` for a
  merely-queued signal. A **remote** (external-pid) or **port** signal bypasses the queue entirely (no
  `trap_exit` policy to defer), so `node_down`/`port_closed` materialise it straight into the mailbox.
### `remote.rs` — remote (external-node) links and monitors
The **local record** of a process's relationships with processes on other nodes; the wire format is
`beam_dist`'s, reached through a trait seam (this crate never depends on `beam_dist`).
- `RemotePid { node: Eterm, number: u32, serial: u32, creation: u32 }` (`Copy`) — a decomposed external
  pid (the `[header, node, creation, data]` thing), holding no boxed pointer into another heap;
  `from_eterm(term) -> Result<RemotePid>` (`BadArg` unless an external pid), `to_eterm(self, &mut Heap)
  -> Result<Eterm>`.
- `PidTarget { Local(ProcessId), Remote(RemotePid) }` — `from_eterm(term) -> Result<PidTarget>`
  (external ⇒ `Remote`, internal ⇒ `Local`, else `BadArg`).
- `RemoteMonitor { reference: MonitorRef, remote: RemotePid, kind: MonitorKind }` — an outgoing monitor
  on a remote process/port.
- `DistSignal { Link { from, to }, Unlink { from, to }, Monitor { watcher, target, reference, kind },
  Demonitor { watcher, target, reference }, Exit { from, to, reason } }` — the control signal the
  runtime routes to the remote node (the `DOP_*` mapping is documented on the module).
- `DistSignalRouter { fn route(&mut self, signal: DistSignal) -> Result<()>; }` — **the distribution
  seam**: implemented downstream (`beam_rt`) over `beam_dist`. `route` must serialise the signal
  **synchronously** (`reason` may live in the signaller's heap, which the caller may drop right after).
  The table's remote records (`remote_links`/`remote_monitors`) and `set_dist_router`/
  `has_dist_router` are on `ptab.rs`. **This seam is OUTBOUND only** (local → remote): there is no
  inbound control-message (`DOP_LINK`/`DOP_MONITOR_P`/…) interpreter here — an inbound handler belongs
  to `beam_rt` (see Known Issues).
### `port.rs` — the port-id table + ownership / link / monitor surface
`PortTable` is the number table the C keeps alongside the pid table, extended with the relationships a
dying port must resolve. Port *behaviour* (driver, scheduling, read path) is `beam_io`'s. The crate-root
re-export keeps `beam_process::PortTable` resolving here.
- `PortMonitor { reference: MonitorRef, watcher: ProcessId }` — a monitor placed on a port.
- `PortClose { owner: Option<ProcessId>, links: Vec<ProcessId>, monitors: Vec<PortMonitor> }` — what
  `close` releases.
- `PortTable { slots: Vec<bool>, free, owners, links, monitors }` — `new()`, `alloc() -> Result<PortId>`
  (`SystemLimit` on exhaustion), `alloc_owned(owner)`, `release(PortId)` (frees the number + drops every
  relationship), `close(PortId) -> Result<PortClose>` (frees the number, returns **and clears**
  owner/links/monitors; unknown id ⇒ `BadArg`), `contains`/`len`.
  - **ownership:** `owner`, `set_owner` (unknown id ⇒ `BadArg`), `clear_owner`.
  - **links:** `link` (deduped — `false` if already linked), `unlink`, `links`.
  - **monitors:** `monitor`, `demonitor`, `monitors`.
  - Every relationship mutation on an unallocated id returns `BeamError::BadArg`.
    `ProcessTable::port_closed` is the one-call orchestrator resolving a closing port's relationships.
### `error.rs` — `pub(crate)` constructors only (no new error enum)
## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only. Never a crate-local error enum, never `panic!`
  on runtime data, never `unwrap()` on runtime data. Unknown pid/name/ref ⇒ `BeamError::TermNotFound`;
  a malformed pid term or out-of-range register index ⇒ `BeamError::BadArg`; pid/port space exhaustion
  ⇒ `BeamError::SystemLimit`; heap exhaustion during delivery ⇒ propagate the underlying
  `BeamError::Alloc` after a collect-and-retry. `beam_term::TermError` is reconciled upstream, so `?`
  crosses the boundary.
- **`unsafe` is confined** to the modules that touch raw words: `signal.rs`/`monitor.rs` message
  builders and `remote.rs` (reading/writing the external-pid thing through `beam_term`'s `unsafe`
  predicates, building tuple/ref words through `beam_alloc::Heap::alloc_words`), plus `src/smp/mod.rs`
  (the two documented `unsafe impl Send`/`unsafe impl Sync for SharedProcessTable`). Every `unsafe fn`
  carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`../beam_common/src/safety.rs` convention). `unsafe_op_in_unsafe_fn` stays clean. `pid.rs`,
  `registers.rs`, `run_queue.rs`, `link.rs`, `monitor.rs` set logic, `ptab.rs` bookkeeping,
  `signal_queue.rs`, `port.rs` and `progress.rs` must be safe Rust. The SMP pool's `unsafe` is limited
  **to those two impls** — `queues.rs`/`worker.rs`/`stats.rs`/`pool.rs` and `timer.rs`/`dirty.rs` are
  safe Rust (synchronisation is `parking_lot` mutexes/condvars + atomics).
- **Never assume a different term layout** — build/inspect terms only through `beam_term`'s public
  accessors + `beam_alloc::Heap`'s builders. Pid/port immediates are bit-exact with
  `beam_term::immediate` (`make_internal_pid`/`pid_number`/`pid_serial`).
- **Reuse, do not reimplement:** the mailbox is `beam_message::Queue`; the copy engine is
  `beam_message::{send, receive, CopyContext}`; the heaps are
  `beam_gc::Collector`/`beam_gc::HeapSpace`/`beam_alloc::Heap`; the off-heap objects are
  `beam_gc::MsoHandle`/`beam_message::OffHeapTable`.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `beam_gc`,
  `beam_message`, `libc`, `parking_lot`. Adding a dependency edge requires reporting to `../`.
  `beam_engine` is **NOT** a dependency (the `ProcessRunner` trait keeps the DAG acyclic).
- Files target ~1000 lines; one responsibility per file. Licensing Apache-2.0. The `src/smp/*` files,
  `src/timer.rs` and `src/dirty.rs` are each well under that figure. `src/ptab.rs` (the table + signal
  hub) is the only file over that figure: ~1535 lines of production code + ~450 lines of tests (~1990
  total). It deliberately stays whole because the signal/link/monitor servicing is tightly coupled to
  `ProcessTable`'s private fields (`slots`/`atoms`/`refs`/`ready`), which a sibling module cannot reach
  — a structural-debt candidate to extract if it grows further or if field visibility is widened.
## Test Strategy
- In-module `#[cfg(test)] mod tests` per module (unit): `pid` (round-trip vs `beam_term` accessors,
  `from_eterm` rejects a non-pid), `registers` (live bounds, range errors, `gather_roots`/
  `scatter_roots` split point), `run_queue` (four-level selection + `remove`/`priority_of`), `link`
  (`add` idempotence), `monitor` (`RefAllocator` monotonic, `MonitorRef::to_eterm` shape), `signal`
  (`exit_policy` table; builder arity + head/tag atoms), `signal_queue` (FIFO, GC round-trip), `port`
  (alloc/reuse + relationship round-trips), `progress` (generation bump), `ptab` (alloc/remove/reuse +
  serial bump, name registry, stale-pid rejection, `deliver` wake list, `take` clearing a pending wake,
  the trap/link-`EXIT`/`{'DOWN'}` paths waking a blocked recipient).
- **`tests/` — the integration evidence pinning the M1 guarantees** (one line each; full assertions in
  the files):
  1. `tests/scheduler.rs` — pid allocation (+ serial bump on reuse, stale-pid rejection); round-robin
     scheduling order (one slice each, requeued at the tail on `Yield`, `Wait` leaves the queue);
     resume-on-send (blocked receiver re-enqueued in the same slice) + a two-process counter ping-pong.
  2. `tests/messaging.rs` — send into a receiver's mailbox, selective `receive` (FIFO, non-matches
     stay queued), `DeliverOutcome::Woke` vs `Enqueued`.
  3. `tests/signals.rs` — link exit propagation (non-trapping dies, trapping survives with
     `{'EXIT', A, Reason}`), monitor down-message, and reason ownership (a structured reason survives
     the recipient's `Process::collect`); trapping-peer, cascaded A→B→C link, `{'DOWN'}`, self-signal.
  4. `tests/gc.rs` — safepoint GC over the process root set (x-register + stack + mailbox payload
     relocate while garbage is reclaimed).
  5. `tests/priority.rs` — four-level order (MAX → NORMAL → LOW, FIFO within a level), requeue at
     **own** priority on `Yield`, woken process resumes at its own priority, `process_flag` PCB support
     (`priority`/`sensitive`/`save_calls`).
  6. `tests/pending_signals.rs` — deferred (schedule-in) vs eager application, FIFO across senders, a
     structured reason surviving `collect`.
  7. `tests/ports.rs` — `PortTable` relationship round-trips + `ProcessTable::port_closed`
     (`{'DOWN', …}` for a monitor, `{'EXIT', Port, Reason}` for a trapping linked process, termination
     for a non-trapping one, unknown port rejected).
  8. `tests/remote.rs` — an external pid links/monitors via the same table entry points, is recorded
     separately, and every change is handed to a recording `DistSignalRouter`; `node_down` resolves then
     purges.
  9. `tests/dict.rs` — `Dict` semantics on the PCB + a structured dict value surviving `collect`.
  10. `tests/fragments.rs` — saved args surviving `collect` + all sub-rootsets relocating in one collect.
  11. `tests/smp.rs` (11 tests) — N-on-M-scheduler progress (M ∈ {1,4}, 200 pids each); work stealing
      (a parked peer steals half of 128 woken receivers); a timer into a blocked receiver;
      `deliver_simple` wake; a dirty `Reschedule` round trip; lifecycle/observables (`stats`/
      `scheduler_stats` one entry per scheduler, `run_queue_len` under a gated runner, `shutdown`+`join`
      returns); a one-scheduler config runs to completion.
  12. `tests/timers.rs` — fire-into-sink with the exact target/message, deadline order (equal deadlines
      in insertion order), `cancel` prevents + is idempotent, `pending`/`next_deadline`, pre-`start`
      scheduling, `shutdown`/`join`.
  13. `tests/dirty.rs` — jobs run on dirty worker threads and every completion is routed verbatim
      (`Reschedule`/`Deliver`), `pending` rises/drains, N-worker concurrency (a `Barrier`), lifecycle +
      submit-after-shutdown, a 0-worker pool buffers and discards at join, sink-after-submit, a
      panicking job does not kill the pool, and a repeated `DirtyPool::start(1, 0)` + `join()` loop.
  14. `tests/progress.rs` — `wait` returns only after every active thread reaches the generation (also
      immediately when already there, and invoking the waker), inactive-thread exclusion (reactivation
      re-includes), concurrent `bump`s each happen once, `block`/`unblock` nest, `safepoint` never blocks
      on the caller's own slot, a clone shares the registry/generation/blocking state.
- **Anti-flakiness (the SMP/timer/dirty/progress suites).** Every assertion is forced by
  **synchronization**, never `std::thread::sleep`: `mpsc` channels + `recv_timeout` carry the completion
  signals, `std::sync::Barrier`/gates coordinate concurrency, and `SchedulerPool::wait_idle(Some(timeout))`
  is the pool's converging idle wait (it also requires the in-flight count to be zero, so counters read
  after it are exact). A wedged pool fails with a clear timeout instead of hanging.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
  beam_process` and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Current total: **220 tests, 0 failures** (131 in-module unit + 88 across the 14 `tests/` files + 1
  doctest). Always go through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never touch
  `../../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness,
  not here.
## Design Decisions
- **A decoupled, ordered per-process signal queue.** A signal is **enqueued** on the receiver's own
  `SignalQueue` at generation time — never applied inline — and applied later, at the target's
  **schedule-in** (`Scheduler::run_slice` calls `ProcessTable::service_signals(pid)` before
  `runner.run`; the scheduler returns early if servicing terminated the pid) or **eagerly at the end of
  the sending operation** when the target is *not* the currently `Running` process (ERTS's early signal
  handling). The reason is **copied into the receiver's own heap at enqueue time**, so the queued signal
  outlives the signaller's heap and its off-heap objects; `exit_policy` is evaluated at **service**
  time, against the receiver's `trap_exit` flag *then*. **Ordering guarantee:** a total FIFO order per
  receiver (a single FIFO), which subsumes ERTS's per-`(sender, receiver)` order; it is **not**
  cross-ordered with the mailbox — a `{'EXIT', …}`/`{'DOWN', …}` becomes a mailbox message only when it
  is *applied* (see `signal_queue.rs` and Known Issues). The queue is an **in-heap sub-rootset**:
  `Process::collect` gathers/scatters it (after the saved args).
- **Resume-on-send: a wake list on the table + absorption by the scheduler.** A delivery (or a signal
  message) to a blocked process must make it runnable, and the single-threaded scheduler must not exit
  `run_ready` while a freshly-woken process is still pending. Rather than have every caller re-enqueue
  (which would double-enqueue a process the `Yield` branch already requeued), the wake is *recorded* on
  the `ProcessTable` (`ready: Vec<ProcessId>`): `Process::deliver` returns `DeliverOutcome::Woke`, the
  table-level `deliver`/signal builders push the pid onto `ready`, and `Scheduler::run_slice` drains it
  via `resume_woken` at the end of **every** slice (and on the empty-queue early return). `resume_woken`
  skips a pid that is no longer live, is not `Runnable`, or is already `in_run_queue` — the last guard
  is what prevents the running process (requeued by `Yield`) from being enqueued twice when it also
  appears on the wake list. A send made **outside** a slice has no slice end to absorb it, so `beam_rt`
  calls `resume_woken` itself (the documented contract above). Both `Waiting` and `Suspended` are woken
  by `Process::wake`: `Waiting` is the ordinary empty-receive case, and waking a `Suspended` process
  keeps the two blocked states symmetric (the message must still be seen; the suspension is lifted
  separately) rather than guessing the historical C nuance — the C's `SUSPENDED`-vs-`suspend`-counter
  distinction is a follow-up.
- **`ProcessRunner` is the engine seam — `beam_engine` is not a dependency.** The scheduler must not
  know how a process executes. `beam_rt`/`beam_engine` implement `ProcessRunner` over the engine's
  `ExecutionContext`, so this crate stays at Stage-5 and the DAG stays acyclic (`beam_engine` →
  `beam_loader` only; `beam_process` → wave-0/1/2 foundations).
- **The `ProcessTable` is the ≥2-process mutation hub.** Links/monitors/signal delivery inherently touch
  two PCBs; holding two `&mut Process` from a `HashMap` is a borrow-checker impossibility, so all such
  operations live on `ProcessTable` and use a **collect-then-apply worklist** (gather the link targets /
  watchers, then mutate each target transiently). `Process` itself holds only its own sets.
- **A signalled reason is COPIED into the receiver's heap at ENQUEUE time.** An exit/link/monitor reason
  that lives in the source process's heap (e.g. a tuple like `{badmatch, X}`) must not be embedded by
  reference into a receiver: the source heap is dropped when it terminates, which would leave a dangling
  pointer in the receiver's queued signal (and a safepoint GC would then corrupt/crash).
  `copy_reason_onto` therefore `copy_struct`s the reason into the receiver's heap **when the signal is
  enqueued** (`enqueue_exit_signal`/`enqueue_down_signal`), registering every referenced off-heap object
  in the receiver's own table, so a queued reason always outlives the signaller. `terminate_into` copies
  the dying process's reason into every peer **before** `drop`ping its PCB; a self-signal (`source ==
  recipient`) is already in the receiver's heap and is not copied. Immediates and literal-area terms are
  shared by `copy_struct`, so the common reasons pay nothing. This mirrors `Process::deliver`'s
  copy-then-enqueue pattern and is what makes `{'EXIT', …}`/`{'DOWN', …}` survive the receiver's next
  collection.
- **GC root set = registers + mailbox payloads + dictionary + saved args + pending signals, gathered
  into a flat `&mut [Eterm]`.** `beam_gc`'s collector takes a flat root slice (`x_reg_array[0..Live)` +
  stack, no frame maps). The mailbox, the process dictionary (`Dict`), the saved BIF/NIF arguments
  (`SavedArgs`) and the pending-signal queue (`SignalQueue`) all hold terms *in this process's heap*, so
  each is a sub-rootset (`beam_gc` explicitly delegates on-heap message queues to `beam_process`).
  `Process::collect` gathers them in a fixed order — registers/stack, mailbox payloads, dictionary,
  saved args, pending signals — runs the collector, then writes each region back in the same order:
  since `beam_message::Message` has no payload setter the queue is **drained** (`Queue::dequeue`), each
  payload appended to the root buffer, and each `Message` **rebuilt** from its relocated payload (via
  the public `Message::new(..).with_token(..).with_off_heap(..)` builder); the dict/saved-args/signal
  regions are scattered back through their own `scatter_roots`. MSO handles are off-heap and do not
  move. This uses only `beam_message`'s public API — no sibling crate is modified.
- **The process dictionary compares keys by raw `Eterm` word equality.** `Dict` is term-agnostic and has
  no `AtomTable`, so exact equality holds for immediates and the same boxed object; structural equality
  of two distinct-but-equal boxed keys (e.g. two separately built 2-tuples) needs `beam_term::compare::eq`
  and is deliberately not a `Dict` dependency. The BIF layer supplies/copies values in and maps the
  `None` (absent) result to the `undefined` atom.
- **Monitor references are on-heap `Ref` things built locally (`monitor.rs`).** `beam_term` has no
  `make_ref` in M1, so a monitor reference is represented as the frozen 4-word ref thing
  `[make_header(3, REF_SUBTAG), node(0), serial, id]` built with `beam_term`'s public `make_header`;
  `beam_message::layout::object_words` copies a ref thing verbatim (`thing_arityval+1` = 4 words, no
  navigable children) and `beam_term::compare` compares internal refs by raw payload words, so a
  `{'DOWN', Ref, …}` message round-trips and matches. When `beam_term` gains a proper `make_ref`, only
  `MonitorRef::to_eterm` changes. This is a documented, contained substitution — flagged to `../`.
- **`Links` may hold the link partner more than once in the C (a link is refcounted); M1 keeps a set.**
  Re-linking an already-linked pair is a no-op (`add` returns `false`) and unlinking removes once, which
  matches the observable `erlang:link/1` / `erlang:unlink/1` semantics for the common cases; the C's
  per-link refcount (for `erlang:monitor` on links and distributed links) is a follow-up.
- **`trap_exit` is a flag on the PCB, not a separate queue.** `exit_policy` reads it — at *service*
  time, when a queued signal is applied — and decides `Terminate`/`Trap`/`Ignore`; the `Trap` case
  builds `{'EXIT', From, Reason}` into the mailbox. This is exactly the observable
  `process_flag(trap_exit, true)` behaviour.
- **`Process` owns a `beam_gc::Collector`, not raw `Heap`s.** The young space is the mutator heap
  (`heap_mut()`), the mature space is a GC target; `should_collect`/`collect` delegate policy to
  `beam_gc::HeapPolicy`. No heap logic is reimplemented here.
- **`parking_lot` is the synchronisation primitive; `libc` remains unused.** The SMP pool, the timer
  service and the dirty pools use `parking_lot::{Mutex, Condvar}` (plus std atomics), and `progress.rs`
  uses `parking_lot::{Mutex, Condvar}` for the barrier. `libc` is declared but unused in the core; it
  stays the documented dependency of the deferred OS-level threading work.
- **The SMP pool reuses the frozen slice (`run_one`/`drain_woken_into`).** Concurrency is added *around*
  the slice, never inside it: `Worker::run` calls the same `pub(crate)` helpers the single-threaded
  `Scheduler` uses, so a process executes identically under either front. The only difference is the
  requeue closure the worker passes — it **buffers** requeues into a local `Vec` instead of touching a
  queue (see the locking discipline).
- **The coarse per-slice table lock + the locking discipline (BINDING).** The pool keeps one
  `Mutex<ProcessTable>` (`SharedProcessTable`) and holds it for a whole slice, so — across all
  schedulers — exactly one process runs at a time (no intra-process parallelism yet). Three lock
  families coexist (queue locks, the table lock, the signals mutex) under the order documented in
  `src/smp/mod.rs`: **never hold a queue lock while taking the table lock and vice-versa**; requeues
  produced while the table guard is held are buffered in `buf` and flushed to the local queue **only
  after** the guard drops; the only nested acquisition is *signals mutex → queue lock* inside
  `PoolSignals::park`/`wait_for_idle` (the reverse never happens).
- **`SharedProcessTable`'s `unsafe impl Send/Sync` is a contained handoff.** `ProcessTable` is `!Send`
  because it owns off-heap `beam_gc::MsoHandle`s whose payload pointer is a `NonNull`. The two `unsafe
  impl`s assert that confining every access to the inner `parking_lot::Mutex` makes moving and sharing
  the wrapper sound; they are the crate's only `unsafe` in the SMP layer. The durable fix belongs
  upstream (make `beam_gc::MsoHandle`/`beam_alloc::RefcBinary` `Send`), which this crate must not change
  — hence a handoff, not a redesign.
- **Non-flaky `wait_idle` via an in-flight counter.** The pool reports idle only when every queue is
  empty **and** the shared `InFlight` count is zero. A pop increments `InFlight` *inside* the same
  critical section that dequeues the pid (so an idle observer cannot slip into the gap), and a worker
  flushes its local `buf` batch **before** calling `finish_slice` (which decrements the counter) — the
  order is load-bearing: flushing after would leave a window where the queues are empty and the count is
  zero while a yielded pid still sits only in `buf`.
- **Loss-free park/notify.** A worker re-checks the queue predicate **under the signals mutex**
  immediately before `Condvar::wait`, and every notifier takes the same mutex before signalling, so a
  wake-up cannot be lost between the "nothing to do" decision and the sleep. The mutex→queue-lock order
  is consistent everywhere (a notifier releases its queue lock before signalling), so the hierarchy
  cannot deadlock.
## Known Issues (intentionally NOT covered in Milestone-1)
- **Multi-scheduler / SMP (remaining gaps)**: lock-free/threaded run queues, scheduler **balancing**, and
  the `+S`/`+SDcpu`/`+SDio` flag parsing are not modelled. The pool holds the table lock for a whole
  slice, so a process **does not run in parallel** across schedulers (one runs at a time). Dirty-job
  **term plumbing** is absent (a `DirtyJob` carries no live heap data) and a timer message must be
  thread-valid (no heap-resident terms). `ThreadProgress` implements the barrier but has **no**
  delayed-dealloc coupling with `beam_alloc`, and **port tasks are not load-balanced** across
  schedulers.
- **The signal queue is not cross-ordered with the mailbox.** A trapped `{'EXIT', …}`/`{'DOWN', …}`
  becomes a mailbox message only when the queued signal is *applied*, so an ordinary send already in the
  mailbox can be seen before it; remote (external-pid) and port signals bypass the queue entirely
  (materialised straight into the mailbox). The C's per-*sender* signal-queue granularity and the
  signal-vs-message ordering barrier remain follow-ups.
- **Priority run queues**: port tasks, and the C's shared NORMAL/LOW physical queue
  `schedule_count`/`RESCHEDULE_LOW` fairness deferral (this crate uses four physically distinct FIFOs —
  see `run_queue.rs`), are follow-ups; moving a process that is *currently sitting in a run queue*
  between levels (the C `erts_set_process_priority` requeue) is also one — a `set_priority` made during
  a slice takes effect at that process's next requeue. The `save_calls` *effect* (actually saving the
  last `N` calls for `seq_trace`/`dbg`) is not modelled — only the `process_flag(save_calls, N)` value
  contract.
- **Process-dictionary interior and heap fragments.** The dictionary is modelled as a flat key/value
  list (no hash-array interior), and `erlang:process_info/1`'s view of it is still absent. A real
  in-heap `ErlHeapFragment` (a `beam_gc`/`beam_message`-owned type, still absent) is not modelled: saved
  args are a plain `Eterm` root list and M1 assumes a message fits the receiver's young heap
  (collect-and-retry once).
- **Distribution-wide links/monitors**: external pids are decomposed into a `remote::RemotePid` and
  recorded (`remote_links`/`remote_monitors`), and every relationship change is routed through the
  `remote::DistSignalRouter` trait seam (implemented downstream over `beam_dist`; no crate dependency).
  **Inbound** decoded control messages (`DOP_LINK`/`DOP_MONITOR_P`/`DOP_EXIT`) are **not** yet fed into
  the table — that interpreter and the `DistSignal`→`Dop` adapter belong to `beam_rt` (a documented
  handoff); with no router installed the table is record-only (`has_dist_router()`).
- **Port lifecycle driving** — port scheduling, port signals and the port's read path — remains
  `beam_io`'s; `PortTable` owns only the id plus the ownership/links/monitors, and
  `ProcessTable::port_closed` delivers the closing port's messages.
- **Exact exit-reason term from `beam_common::ExitReason`**: signal reasons are carried as live `Eterm`
  terms (the deliverable form), not the term-independent `beam_common::ExitReason`; the mapping between
  them belongs to the raising BIF layer (`beam_bifs`).
## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/pid.rs` → `ProcessId`/`PortId` (bit-exact internal pid/port immediates) + id limits
- `src/registers.rs` → `XRegs`, `Stack`, `Registers` (the root-set containers); `MAX_REG`/`MAX_ARG`
- `src/process.rs` → `Process` (PCB), `ProcessState`, `ProcessFlags` (incl. `sensitive`), `DeliverOutcome`, `Process::wake`, the priority/`save_calls` accessors, message delivery + GC entry + the dict/saved-args/signal sub-rootsets
- `src/dict.rs` → `Dict` (process dictionary: put/get/erase/get_keys + GC gather/scatter)
- `src/fragments.rs` → `SavedArgs` (saved BIF/NIF argument roots + GC gather/scatter)
- `src/run_queue.rs` → `Priority` (max/high/normal/low) + `RunQueue` (four per-priority FIFOs + the LOW/NORMAL fairness-deviation note + `split_off_half` for work stealing)
- `src/scheduler.rs` → `Scheduler`, `RunOutcome`, `ProcessRunner` (the engine seam), `enqueue_with_priority`, `resume_woken` (wake absorption), the schedule-in signal servicing, and the shared `pub(crate)` slice helpers `run_one`/`drain_woken_into` (reused by the SMP pool)
- `src/smp/mod.rs` → the SMP module docs (incl. the BINDING locking discipline), `SchedulerConfig`, `SharedProcessTable` (with the two documented `unsafe impl Send/Sync`)
- `src/smp/queues.rs` → `Queues` (shared injector + per-scheduler local queues + `InFlight`; `Acquired`; the own→injector→steal acquisition + length/idle predicates)
- `src/smp/worker.rs` → `Worker` (the scheduler-thread loop) + `PoolSignals` (work/idle condvars + stop; loss-free park) + `record_error`
- `src/smp/stats.rs` → `SchedulerStats`, `SchedulerStat` (public snapshots) + the internal `WorkerCounters`/`InFlight` atomics
- `src/smp/pool.rs` → `SchedulerPool` (start/spawn/enqueue/deliver/timer/dirty/stats/idle/safepoint/shutdown/join + the `TimerSink`/`DirtyCompletionSink` impls and the lifecycle rules)
- `src/timer.rs` → `TimerService` (one thread over a deadline min-heap), `TimerRef`, `TimerSink` (the high-level `send_after`/`cancel` service)
- `src/dirty.rs` → `DirtyPool`, `DirtyKind`, `DirtyOutcome`, `DirtyJob`, `DirtyCompletionSink` (the dirty-CPU + dirty-IO worker pools)
- `src/progress.rs` → `ThreadProgress` (the cross-scheduler barrier), `ManagedThread` (the per-thread progress slot)
- `src/signal.rs` → `SignalAtoms` (incl. `port`), `ExitSignal`, `ReasonClass`, `ExitPolicy`/`exit_policy`, the signal-message builders (exit + `process`/`port`/remote `DOWN`)
- `src/signal_queue.rs` → `SignalQueue` + `PendingSignal`/`PendingSignalKind` (the decoupled pending-signal queue + its GC gather/scatter)
- `src/link.rs` → `Links` (bidirectional link set)
- `src/monitor.rs` → `MonitorRef`, `MonitorKind`, `Monitor`, `MonitoredBy`, `Monitors`, `RefAllocator`
- `src/remote.rs` → `RemotePid`, `PidTarget`, `RemoteMonitor`, `DistSignal`, `DistSignalRouter` (the distribution seam)
- `src/port.rs` → `PortTable` (id alloc + ownership/link/monitor surface), `PortMonitor`, `PortClose`
- `src/ptab.rs` → `ProcessTable` (pid alloc/lookup + name registry + signal/link/monitor hub + the decoupled signal servicing + the wake list + the remote records + the distribution seam), `ExitEffect`
- `src/error.rs` → `pub(crate)` error constructors (no new error type)
- `tests/scheduler.rs` → pid allocation + round-robin scheduling order + resume-on-send + counter ping-pong
- `tests/messaging.rs` → send into a mailbox + selective receive + `DeliverOutcome` wake reporting
- `tests/signals.rs` → link exit propagation (+ `trap_exit`) + monitor down-message + reason ownership
- `tests/gc.rs` → safepoint GC over the process root set
- `tests/dict.rs` → dictionary semantics + a structured dict term surviving `Process::collect`
- `tests/fragments.rs` → saved args surviving `collect` + all sub-rootsets relocating in one collect
- `tests/priority.rs` → four-level scheduling order + requeue-at-own-priority + the `process_flag` PCB support
- `tests/pending_signals.rs` → deferred-vs-eager signal application + FIFO order + a structured reason surviving `collect`
- `tests/ports.rs` → `PortTable` ownership/link/monitor round-trips + `ProcessTable::port_closed`
- `tests/remote.rs` → external-pid link/monitor recording + the `DistSignalRouter` seam + `node_down`
- `tests/smp.rs` → the `SchedulerPool`: N-on-M-scheduler progress, work stealing, a timer/dirty/send wake, and the lifecycle/observables (non-flaky via `recv_timeout`/gates/`wait_idle`)
- `tests/timers.rs` → `TimerService`: fire-into-sink, deadline/insertion order, cancel, `pending`/`next_deadline`, pre-start scheduling, shutdown/join
- `tests/dirty.rs` → `DirtyPool`: dirty-worker execution + verbatim completion routing, `pending`, N-job concurrency, lifecycle, 0-worker buffering, sink-after-submit, panicking job, and the repeated start/join regression
- `tests/progress.rs` → the `ThreadProgress` barrier: wait-until-generation, inactive-thread exclusion, concurrent bumps, block/unblock nesting, own-slot non-blocking, the waker, clone sharing
- `../../erts/emulator/beam/erl_process.{c,h}` → PCB, scheduler, run queues (read-only ground truth)
- `../../erts/emulator/beam/erl_thr_progress.c` → the thread-progress barrier ground truth (read-only)
- `../../erts/emulator/beam/erl_hl_timer.c` → the high-level timer wheel ground truth (read-only)
- `../../erts/emulator/beam/erl_ptab.{c,h}` → process/port table + name registry (read-only)
- `../../erts/emulator/beam/erl_monitor_link.c` → links/monitors (read-only)
- `../../erts/emulator/beam/erl_proc_sig_queue.c` → the signal-queue ground truth the decoupled pending-signal queue reproduces (read-only)
- `../../erts/emulator/beam/erl_vm.h` → `MAX_REG`/`MAX_ARG` (read-only)
- `../beam_gc/`, `../beam_message/`, `../beam_alloc/` → the collector, mailbox/copy engine and heap this crate drives (siblings — read-only, escalate writes to `..`)
## Notes for Agents
- Read `../CONTEXT.md` (crate-layer contracts, the `Process`/`Scheduler` seam) and `../../CONTEXT.md`
  §1 A.1/§3/§5 before changing this crate. Ground truth is the legacy C listed above — **read it; never
  modify it.**
- The **SMP pool lives in `src/smp/`** and must follow the **locking discipline** documented in
  `src/smp/mod.rs` (never hold a queue lock while taking the table lock and vice-versa; buffer requeues
  while the table guard is held; the only nested order is *signals mutex → queue lock*). It reuses the
  frozen `run_one`/`drain_woken_into` slice helpers — do **not** re-implement slice logic in a worker.
- `ProcessTable` is `!Send` (off-heap `beam_gc::MsoHandle` `NonNull` payloads), so moving it into the
  pool needs `SharedProcessTable`'s two `unsafe impl Send/Sync` — they are the **contained workaround**
  (all access is serialised by the inner mutex), pending an upstream `beam_gc`/`beam_alloc` fix that
  makes the off-heap handles `Send`. Do not widen this `unsafe`.
- Build/test with the manifest path: `erts_rust/scripts/cargo.sh test --manifest-path
  erts_rust/Cargo.toml -p beam_process` (sandbox `$HOME` is read-only — always the wrapper). Never
  `cd`; never touch `../../tools/`.
- `Process::heap_mut()` returning two disjoint `&mut` fields (heap + mailbox) requires destructuring
  `self` when calling `beam_message::send` — do not add a combined raw-pointer accessor.
- `beam_alloc::Heap` exhaustion (`AllocKind::Heap`) from the young heap means **collect and retry** (see
  `beam_alloc`/`beam_gc` Design Decisions), not fatal OOM — `Process::deliver` implements that retry.
- The collector moves live terms, so **every** in-heap reference must be a root. `Process::collect`
  scans registers/stack, mailbox payloads, the process dictionary (`Dict`), the saved args (`SavedArgs`)
  and the pending-signal queue (`SignalQueue`) — add any NEW in-heap sub-rootset to its gather/scatter,
  in the same order, in the same change.
## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types and the wave-3 dependency position.
- `../../CONTEXT.md` → workspace contracts (§1 A.1 process semantics, §3 error strategy, §5 GC/memory model, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A/§D → process/scheduler structure and the memory model.
- `../beam_gc/CONTEXT.md` (the collector + MSO this crate drives), `../beam_message/CONTEXT.md` (the
  mailbox `Queue` + `CopyContext`), `../beam_alloc/CONTEXT.md` (the per-process `Heap`), `../beam_term/CONTEXT.md` (the pid/port immediates).
