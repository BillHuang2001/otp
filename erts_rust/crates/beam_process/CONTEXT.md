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
**Not this crate's ground truth** (a C-deletion record must not attribute them here): the allocators
(`erl_alloc*.c`, `erl_alloc_util.c`, `erl_goodfit/bestfit/afit/ao_firstfit_alloc.c` → `beam_alloc`);
the message copy engine (`erl_message.c`/`copy.c` → `beam_message`); crash dumping
(`erl_process_dump.c`/`erl_crash_dump.c` → `beam_rt`'s `crash_dump.rs`); microstate accounting
(`erl_msacc.c`) and flexible counters (`erl_flxctr.c`) are **not modelled by any crate** (`beam_rt`
lists the `mc_*`/`scheduler_wall_time` BIFs as a native gap); `erl_thr_queue.c`,
`erl_sched_spec_pre_alloc.c` and `erl_process_lock.c` are **not modelled**; and `time.c`/`erl_time_sup.c`
(low-level time + Erlang time services) are served by `beam_bifs`'s time BIFs / `beam_rt`, not here
(this crate's `timer.rs` is the `erl_hl_timer.c` analogue).
## Milestone-1 scope
The crate has **two scheduler fronts**: the single-threaded `Scheduler` loop (whose slice semantics are
frozen) and the SMP `SchedulerPool` that runs `N` scheduler threads over one shared table. Both execute
the **same** slice logic — `scheduler::{slice_begin, slice_end}` (with `run_one` = both halves around one
full-budget runner call) plus `drain_woken_into` — so the SMP front adds *concurrency* (and a bounded
lock hold), not a second scheduler implementation. The pool's table lock is held for one reduction
**quantum** (`SchedulerConfig::set_lock_quantum`, default `DEFAULT_LOCK_QUANTUM = 256`), not a whole
slice, so an independent process / cross-thread send / timer / dirty completion is serviced *inside*
another process's slice — but exactly one process still **executes** at a time across all schedulers.
"Correct" means the pid table, the **four-level** run-queue order, message delivery, the **decoupled
per-process signal queue**, the monitor/registration surface, the process dictionary and saved-argument
sub-rootsets, the **port ownership/link/monitor** surface, **remote links/monitors** through the
`DistSignalRouter` seam, and a safepoint GC over the process root set all behave per the C/oracle
semantics for the common cases, and are covered by tests. What is deliberately *not* modelled is listed
under Known Issues.
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
  (outgoing process monitors), `monitored_by: Vec<MonitoredBy>` (incoming),
  `port_monitors: Vec<PortWatch>` (outgoing port watches), `exit_reason: Option<Eterm>`.
  - `new(pid) -> Result<Process>` / `with_policy(pid, policy)` /
    `with_policy_and_priority(pid, policy, Priority)` — the **spawn-at-priority** constructor (a runtime
    composes `alloc_pid` + it + `ProcessTable::insert`, so a child is born at its level, not Normal).
  - **`status`:** `ProcessState::status_atom(&self) -> Option<&'static str>` and `Process::status()` map
    the state — with the `garbage_collecting` flag layered over it — to the oracle's
    `process_info(P, status)` atoms (`"running"`/`"runnable"`/`"waiting"`/`"suspended"`/`"exiting"`);
    `None` for `Terminated` (a dead pid answers `undefined` — the BIF layer supplies that).
  - `wake(&mut self) -> bool` — `Waiting`/`Suspended` ⇒ `Runnable` + `true` (the blocked-recipient
    wake used by resume-on-send).
  - `heap()/heap_mut()` (the **young** mutator heap, `collector.young_mut().heap_mut()`); the
    `priority`/`save_calls`/`sensitive`/`trap_exit` accessors; `mailbox(_mut)`, `registers(_mut)`,
    `dict(_mut)`, `saved_args(_mut)`, `signals(_mut)`/`pending_signals()`, `off_heap(_mut)`,
    `links(_mut)`, `monitors(_mut)`, `monitored_by(_mut)`, `port_monitors(_mut)`,
    `exit_reason`/`set_exit_reason`.
  - **split borrows** (each destructures `self`; safe Rust, no raw pointer, so the compiler proves the
    two `&mut` are disjoint fields of `Collector`/`Process`): `heap_and_off_heap_mut(&mut self) -> (&mut
    Heap, &mut OffHeapTable)` hands out the young heap and the off-heap table together, so a caller can
    drive `beam_message::Fragments::move_into(dst_heap, dst_off_heap)` in one borrow — the only way to
    move a **fragmented** message payload onto the process's heap; `heap_and_mailbox_mut(&mut self) ->
    (&mut Heap, &mut Queue)` hands out the young heap and the mailbox together, so a caller can drive
    `beam_message::send_fragmented(from, term, to_heap, to_queue, ctx, n)` without reimplementing the
    fragment path. These are the only two `*_mut` pairs with a real caller need (the rest were audited);
    do not add a combined raw-pointer accessor for any pair.
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
  `get_keys_for_value`, `entries()`; `new()` (empty) / `with_atoms(Rc<AtomTable>)`.
  - **GC pair:** `gather_roots(out) -> usize` (appends each entry's key then value) /
    `scatter_roots(roots, start)` (never-panic).
  - **Semantics:** `None` is the C `THE_NON_VALUE` (absent; the BIF layer maps it to `undefined`).
    Keys — and, for `get_keys_for_value`, **values** — compare with `beam_term::compare::eq`, i.e. the
    C's `EQ` (`erl_utils.h:113`): structural (`=:=`), so a *boxed* key matches a distinct-but-equal
    term (`put({a},1), get({a})` ⇒ `1`). The comparator resolves atom names through the `Rc<AtomTable>`
    handle the `Dict` carries (`new()` ⇒ an empty table, whose unknown-name fallback is the raw atom
    index — the C's `EQ` for two immediates). Holds terms resident in THIS process's young heap (a
    `Process::collect` sub-rootset).
  - **Iteration order (documented deviation):** the pairs are kept in insertion order, so
    `get_keys`/`entries` are insertion-ordered. The C walks its hash array (`pd_hash_get_all`) and its
    order is a hash-layout artifact (`put(a,1), put(b,2), get()` ⇒ `[{b,2},{a,1}]` on OTP 29.1.1);
    reproducing it means porting the hash array — the O(1)-lookup optimization this module defers.
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
  `iter`/`iter_at`; `clear`; `reprioritize(pid, Priority) -> bool` — move an already-queued pid to the
  tail of the new level (never double-enqueues, never inserts a non-queued pid): the run-queue half of
  `Scheduler::set_priority`.
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
  - `set_priority(&mut self, table, pid, priority) -> Result<()>` — update the PCB priority and, **iff**
    the pid is already queued at a different level, move it to the new level's tail; the C
    `erts_set_process_priority` requeue. `TermNotFound` for an unknown pid leaves the queue untouched.
- **Reduction accounting is delegated to the runner.** `DEFAULT_REDUCTIONS_PER_SLICE` is handed to
  `ProcessRunner::run` as its `budget` argument; the scheduler never reads or resets
  `Process::reductions` (`process.rs:367-379` — the accessors are exercised only by tests). The runner
  owns the reds count and decides when to return `RunOutcome::Yield`. **Pre-emption is therefore
  cooperative**: `tests/preemption.rs` proves a budget-respecting runner interleaves several CPU-bound
  processes round-robin with no starvation, but the M1 `beam_rt::RuntimeRunner` ignores its `budget` and
  never yields, so it is a delivered-but-unconsumed seam (see Known Issues).
- **Shared slice helpers (the SMP reuse seam).** `pub(crate) slice_begin(pid, table) -> Result<bool>`
  (mark executing + apply the queued signals) and `pub(crate) slice_end<F>(pid, table, outcome,
  enqueue)` are the frozen schedule-in/schedule-out halves; `pub(crate) run_one<R, F>(pid, table,
  runner, reductions, enqueue)` = `slice_begin` + one full-budget runner call + `slice_end`, and
  `pub(crate) drain_woken_into<F>(table, enqueue)` absorbs the wake list. `run_slice`/`resume_woken`
  delegate to them. The SMP pool calls the same helpers (splitting a slice into lock quanta around
  them) with a closure that **buffers** requeues instead of touching a queue, so both fronts run
  identical slice logic. `DEFAULT_LOCK_QUANTUM = 256` is the default pool lock hold — see
  `src/smp/CONTEXT.md`.
- **The single-threaded orchestration contract `beam_rt` follows** (`Scheduler::resume_woken` is the
  only coupling): spawn ⇒ `table.spawn_default()?` then `scheduler.enqueue(pid)`; a runner that blocks
  on an empty receive sets `ProcessState::Waiting` itself **before** returning `RunOutcome::Wait`; a
  send from **inside** a runner ⇒ `table.deliver(_simple)(…)` (absorbed at slice end); a send from
  **outside** a slice ⇒ `table.deliver(_simple)(…)` then `scheduler.resume_woken(&mut table)`; run ⇒
  `scheduler.run_ready(&mut table, &mut runner, None)`.
### `smp/` — the multi-scheduler (SMP) pool
The `Scheduler` counterpart for SMP: `N` scheduler threads over one shared process table, per-scheduler
run queues + work stealing, the `ThreadProgress` barrier, the `TimerService` and the two `DirtyPool`s.
Crate-root re-exports: `beam_process::{SchedulerPool, SchedulerConfig, SchedulerStats, SchedulerStat,
SharedProcessTable}`. Full API surface, the **BINDING locking discipline** (three lock families, the
buffer-requeues rule), the **bounded-quantum lock** rule (`run_locked_quanta`, `set_lock_quantum`) and
the `unsafe impl Send/Sync` handoff live in **`src/smp/CONTEXT.md`** — read it before changing
anything here. `SchedulerPool::set_priority(pid, priority)` implements the same requeue as the
single-threaded `Scheduler` (PCB update under the table lock, then an `O(schedulers)` search of the
injector + every local queue, one queue lock at a time).
### `timer.rs` — the high-level timer service
- `TimerService` — one background thread over a deadline min-heap: `new()`, `start(Arc<dyn TimerSink>)`
  (idempotent), `send_after(Duration, ProcessId, Eterm) -> TimerRef`, `cancel(TimerRef) -> bool`,
  `pending()`, `next_deadline() -> Option<Instant>`, `is_running()`, `shutdown()` (terminal), `join()`.
  This is the delivery **mechanism** only: it schedules one message to one `ProcessId` after a delay.
  The `erlang:send_after/3`, `start_timer/3` (`{timeout,Ref,Msg}` needs `make_ref`), `cancel_timer/1,2`,
  `read_timer/1`, `start_timer/4` and `receive ... after T` timeout-label semantics are BIF/engine-side
  (owned by `beam_bifs`/`beam_rt`); `read_timer` and a hibernate state are absent here.
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
- `SignalAtoms { exit, down, process, port, normal, kill, killed, noproc }` — the well-known atom
  indices used to build signal messages; `from_table(&mut AtomTable)` interns `'EXIT'`/`'DOWN'`/
  `process`/`port`/`normal`/`kill`/`killed`/`noproc`; accessors incl. `port()`, `noproc()`/`noproc_term()`,
  `normal_term()/kill_term()/killed_term() -> Eterm`.
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
  `build_down_message(heap, atoms, user_tag: Option<Eterm>, reference, pid, reason)`
  (`{'DOWN', Ref, process, Pid, Reason}`, or `{Tag, Ref, process, Pid, Reason}` when `user_tag` is
  `Some`); `build_down_message_port(...)` (the `port`-**kind** form); `build_down_message_remote(...)`
  (materialises the external-pid target on the same heap); and the fully general
  `build_down_message_term(heap, atoms, user_tag: Option<Eterm>, reference, kind: Atom, target: Eterm,
  reason)`. `kind` is the `{'DOWN', …}` 3rd-element **kind** atom (`process`/`port`), distinct from the
  caller's `user_tag` (which replaces the message's head atom).
### `signal_queue.rs` — the decoupled per-process pending-signal queue
A signal is *queued* on the receiver and applied later (see the eager-vs-queued rule under `ptab.rs`);
its `reason` is copied into the receiver's own heap at enqueue time, so the queue is an in-heap
sub-rootset.
- `PendingSignalKind { Exit, Down }`.
- `PendingSignal { Exit { from, reason }, Down { reference: MonitorRef, target: ProcessId, kind:
  MonitorKind, tag: Option<Eterm>, reason } }` (`Copy`) — the queued signal; `exit(..)`/`down(..)`
  constructors, `kind()`, `tag()`/`reason()`. The `tag` is the caller's `monitor/3` user tag, carried so
  the deferred `{'DOWN', …}` materialises with the right head atom.
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
- `MonitorKind { Process, Port }` — `type_name() -> &'static str` (`"process"`/`"port"`) and
  `from_type_name(&str) -> Option<MonitorKind>` back the `erlang:monitor/2` type dispatch (any other
  type name ⇒ `None` ⇒ the caller's `badarg`, matching the oracle).
- `PortWatch { reference: MonitorRef, port: PortId }` (`Copy`) — the **outgoing** half of a port
  monitor, held in `Process::port_monitors`. Plain values only (see Design Decisions: not a GC root).
- `Monitor { reference, target: ProcessId, kind }` (outgoing) / `MonitoredBy { reference, watcher:
  ProcessId, kind }` (incoming). These three structs' field sets are **frozen** — they are constructed
  by struct literal from `beam_rt`, so no field may be added; the `monitor/3` user **tag** therefore
  lives in a `ProcessTable` side table, not here (see Design Decisions).
- `Monitors { entries: Vec<Monitor> }` — `push`, `remove(reference) -> Option<Monitor>`,
  `find(reference)`, `iter`, `drain`.
- `payload_is_down_for(payload: Eterm, reference: MonitorRef, down_atom: Atom) -> bool` — the guarded
  `{'DOWN', Ref, _, _, _}` shape test `demonitor_flush` scans mailboxes with (one `unsafe` block, no
  out-of-bounds read).
- `RefAllocator { next: u64 }` — `next() -> MonitorRef` (monotonic, never reuses an id). Owned by
  `ProcessTable`.
### `ptab.rs` — the process/port table + name registry + signal/link/monitor hub
The single place that mutates ≥2 processes (mirrors the C ptab lock domain); M1 single-threaded, so it
holds `Process` values directly. (`PortTable` lives in `port.rs`.)
- `ProcessTable { slots, free, names: HashMap<Atom, ProcessId>, refs: RefAllocator, atoms: SignalAtoms,
  ready: Vec<ProcessId>, remote_links, remote_monitors, named_monitors: HashMap<MonitorRef,
  NamedTarget>, local_node: Option<Atom>, router: Option<Box<dyn DistSignalRouter>> }`
  (`Slot { serial: u64, process: Option<Process> }`; `NamedTarget { name: Atom, node: Atom }`). `ready`
  is the **wake list** (a delivery/signal pushes a woken pid; `drain_woken` returns **and** clears it);
  `router` is the distribution seam (`None` ⇒ record-only).
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
    `terminate(pid, reason)`; the additive `terminate_with_ports(&mut PortTable, pid, reason)` sibling
    threads the port table so a dying process's outgoing `port_monitors` also remove their matching
    **incoming** `PortMonitor` entries (`terminate` is its no-ports delegation — see `port.rs`/Design
    Decisions).
  - **monitors:** `monitor(watcher, target: ProcessId, kind) -> Result<MonitorRef>` — a **non-live**
    target with a live watcher yields `Ok(ref)` plus an immediate
    `{'DOWN', Ref, process, Target, noproc}` in the watcher's mailbox (the oracle's `noproc`);
    `demonitor(watcher, reference) -> Result<bool>`; `demonitor_flush(watcher, reference) ->
    Result<bool>` (also removes a queued `{'DOWN', …}` for that reference —
    `erlang:demonitor(Ref,[flush])`); `monitor_named(watcher, name: Atom, node: Atom, kind)` plus
    `set_local_node(Atom)`/`local_node()` (`erlang:monitor(process, Name)`: the DOWN target is the
    2-tuple `{Name, node}`; with no local node installed ⇒ typed `unsupported`);
    `monitor_port(watcher, port: PortId, &mut PortTable)`, `monitor_port_term(watcher, target: Eterm,
    &mut PortTable)`, `demonitor_port(watcher, reference, &mut PortTable)` (`erlang:monitor(port, Port)`).
    `monitor_term` dispatches a pid → `monitor`, an atom name → `monitor_named`, an external pid → the
    remote record + route; a **port term** with `MonitorKind::Process` ⇒ `BadArg` (oracle) and with
    `MonitorKind::Port` ⇒ typed `unsupported` (that entry point has no `&mut PortTable` — use
    `monitor_port_term`); anything else ⇒ `BadArg`.
  - **`erlang:monitor/3` user tag (additive):** the parallel entry points `monitor_with_tag`,
    `monitor_named_with_tag`, `monitor_port_with_tag`, `monitor_port_term_with_tag` and
    `monitor_term_with_tag` take a trailing `tag: Option<Eterm>` (the existing no-tag methods delegate
    with `None`). A `Some(tag)` makes the delivered message `{Tag, Ref, Kind, Target, Reason}` instead
    of `{'DOWN', Ref, Kind, Target, Reason}` — the tag replaces the head atom only. The tag is stored in
    the private `tags: HashMap<MonitorRef, Eterm>` side table (`remember_tag`/`forget_tag`, cleaned up at
    `demonitor`/`demonitor_port`/termination/port close) — **never** in the frozen `Monitor`/`MonitoredBy`/
    `PortWatch` structs. The tag must be a heap-independent **immediate**; a non-immediate tag (or any tag
    on a **remote** external-pid target — `DOP_MONITOR_P` carries no tag) is a typed `unsupported`.
  - **decoupled signal servicing:** `service_signals(pid) -> Result<()>` — drains + applies the pid's
    pending-signal queue in FIFO order, cascading to any process those signals affect (the scheduler
    calls it at schedule-in); the additive `service_signals_with_ports(&mut PortTable, pid)` sibling
    threads the port table so a schedule-in cascade that terminates a process also cleans its incoming
    port monitors (the plain form delegates with no ports); `pending_signals(pid)`,
    `signals(pid) -> Result<&SignalQueue>`.
  - **arbitrary pid terms:** `link_term`/`unlink_term(a, b: Eterm)`, `monitor_term(watcher, target:
    Eterm, kind) -> Result<MonitorRef>`, `send_exit_term(from, to: Eterm, reason)` — a local pid routes
    to the local op, an external pid is recorded + routed (see `remote.rs`); `remote_links(pid)`/
    `remote_monitors(pid)`.
  - **distribution seam:** `set_dist_router(Box<dyn DistSignalRouter>)`, `take_dist_router()`,
    `has_dist_router() -> bool`.
  - **process-death observer seam:** `set_process_exit_observer(Box<dyn ProcessExitObserver>)`,
    `take_process_exit_observer() -> Option<Box<dyn ProcessExitObserver>>`,
    `has_process_exit_observer() -> bool` (mirrors the distribution seam) — the hook a host installs to
    release per-process resources this crate does not own (ETS tables owned by the dying pid are the
    motivating case). See `exit_observer.rs`.
  - **whole-node down / port close:** `node_down(node: Eterm, reason)` — resolves + purges every remote
    relationship with `node` (a `{'DOWN', …}` per monitor, a trapped/fatal `{'EXIT', …}` per link);
    `port_closed(&mut PortTable, port, reason)` — drains the port's owner/links/monitors and delivers
    the `{'DOWN', Ref, port, Port, Reason}` / `{'EXIT', Port, Reason}` shapes, and terminates any
    non-trapping linked peer through `terminate_with_ports` so that peer's own port monitors are cleaned
    too. This is the **port-died** direction; the reverse (a **watcher process** dies) is handled in
    `terminate_into` by draining `Process::port_monitors` and calling `PortTable::demonitor` on the
    matching incoming entries — but only when a port table was threaded in (see `terminate_with_ports`).
  - **name registry:** `register(name, pid) -> Result<()>` (`BadArg` for a taken name **or a non-live
    pid**), `unregister(name) -> Result<()>` (`BadArg` for an unknown name), `whereis(name) ->
    Option<ProcessId>`, `registered()`, `name_of(pid) -> Option<Atom>` (the reverse lookup a
    `process_info(P, registered_name)` BIF needs; live-only).
  - **liveness / reason:** `is_process_alive(pid) -> bool` (table membership), `exit_reason(pid) ->
    Result<Option<Eterm>>` (live-only).
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
### `exit_observer.rs` — the process-death observer seam
The host hook for per-process resources this crate does not own (the **ETS tables a pid owns** are the
motivating case). Mirrors the `DistSignalRouter` pattern: a trait object on the table, implemented
downstream. The table stays free of any dependency on the observing subsystem; with **no** observer
installed (`None`) behaviour is byte-identical to before the seam existed.
- `ProcessExitObserver { fn process_exited(&mut self, exit: &ProcessExit<'_>); }` — called **exactly
  once per death** from `terminate_into`, after the death has been propagated (links/monitors
  signalled) and **before** `drop(process)` (so `reason`, which may live in the dying heap, is still
  valid). No `Send` bound (consistent with `router`).
- `ProcessExit<'a>` — the payload, **private fields + read-only accessors**; built only by the finalizer
  (`pub(crate) fn new`). Fields: `pid: ProcessId` (the key an observer uses to release its own per-pid
  state; an immediate ⇒ retainable), `reason: Eterm` (the exit reason exactly as received; may live in
  the dying heap, so an observer that retains it must copy it), `name: Option<Atom>` (the registered
  name, captured **before** the registry entry is dropped; an immediate ⇒ retainable; without it the
  registration would be unobservable), `port_monitors: &'a [PortWatch]` (the drained outgoing port
  watches; `PortWatch` is plain values ⇒ heap-independent).
- **Failure policy (documented in the module rustdoc):** an absent observer is a no-op; a **panicking**
  observer is contained with `std::panic::catch_unwind(AssertUnwindSafe(...))` (the observer gets only
  an immutable payload and holds no reference to the table, so a panic cannot half-update the table —
  only the observer's own state), reported via `beam_common::beam_error!`, and termination still
  completes. Implementations should be cheap (they run on the finalizing scheduler thread) and prefer
  returning over panicking for expected conditions.
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
  (the two documented `unsafe impl Send`/`unsafe impl Sync for SharedProcessTable` — still required at
  HEAD; the measured `!Send` causes are listed under Design Decisions). Every `unsafe fn`
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
- **`tests/` — the integration evidence pinning the M1 guarantees.** **25 files**, one topic each; the
  authoritative per-file list lives in **`tests/CONTEXT.md`**. In short: `scheduler`/`messaging`/
  `signals`/`gc`/`priority`/`pending_signals`/`dict`/`dict_oracle`/`fragments` cover the single-threaded
  scheduler, the mailbox, signals, PCB state and the dictionary (including its `erl`-cross-checked
  structural key/value equality); `ports`/`remote` the port table and distributed links/monitors;
  `process_exit_observer`/`port_monitor_cleanup` the process-death observer seam and the death-side
  port-monitor cleanup (incl. `erl`-cross-checked oracle probes); `smp`/`lock_scope`/`timers`/`dirty`/
  `progress` the SMP pool; `registry_monitors`/`monitor_tag` the
  monitor/registration semantics (incl. `noproc`/flush/name targets, the `monitor/3` user tag, and `erl`
  cross-checks); `preemption` the measured round-robin fairness; `split_borrow` the two split borrows
  (`heap_and_off_heap_mut` storage identity + `Fragments::move_into`; `heap_and_mailbox_mut` +
  `send_fragmented`); and
  `capability_inventory`/`process_info_source`/`spawn_options` the typed-error inventory, the
  `process_info` item→accessor map and the spawn-option composition. `tests/lock_scope.rs` is the
  bounded-quantum-lock evidence (a slice = `ceil(budget/quantum)` runner invocations under one slice; a
  cross-thread send and a peer run *inside* another process's slice — both assertions fail under a
  whole-slice lock) plus the 1-vs-4-scheduler determinism check.
- **Anti-flakiness (the SMP/lock-scope/timer/dirty/progress suites).** Every assertion is forced by
  **synchronization**, never `std::thread::sleep`: `mpsc` channels + `recv_timeout` carry the completion
  signals, `std::sync::Barrier`/gates coordinate concurrency, and `SchedulerPool::wait_idle(Some(timeout))`
  is the pool's converging idle wait (it also requires the in-flight count to be zero, so counters read
  after it are exact). `tests/lock_scope.rs` is the only file with wall-clock bounds; they are loose and
  two-sided (a cross-thread send must be serviced far below the slice's duration, and the slice must last
  far above that bound — the anti-vacuity guard). A wedged pool fails with a clear timeout instead of
  hanging.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
  beam_process` and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Current total: **351 passed / 0 failed / 1 ignored** (160 in-module unit + 190 integration across the
  25 `tests/` files + 1 doctest; the ignored test is the documented non-yielding-runner hang failure mode
  in `preemption.rs`), warning-free. Always go through `../../scripts/cargo.sh` (`$HOME` is read-only),
  never `cd`, never touch `../../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness,
  not here.
## Design Decisions
- **A decoupled, ordered per-process signal queue.** A signal is **enqueued** on the receiver's own
  `SignalQueue` at generation time — never applied inline — and applied later, at the target's
  **schedule-in** (`run_slice` calls `service_signals` before `runner.run`, returning early if servicing
  terminated the pid) or **eagerly at the end of the sending operation** when the target is *not* the
  currently `Running` process. The reason is **copied into the receiver's heap at enqueue time**, so a
  queued signal outlives the signaller's heap; `exit_policy` is evaluated at **service** time against the
  receiver's `trap_exit` *then*. **Ordering:** a total FIFO order per receiver, subsuming ERTS's
  per-`(sender, receiver)` order; it is **not** cross-ordered with the mailbox.
- **Resume-on-send: a wake list on the table + absorption by the scheduler.** The wake is *recorded* on
  the `ProcessTable` (`ready: Vec<ProcessId>`) rather than re-enqueued by every caller (which would
  double-enqueue a pid the `Yield` branch already requeued): `Process::deliver` returns
  `DeliverOutcome::Woke`, the table-level deliver/signal builders push onto `ready`, and `run_slice`
  drains it via `resume_woken` at the end of **every** slice (and on the empty-queue early return).
  `resume_woken` skips a pid that is dead, not `Runnable`, or already `in_run_queue` — the last guard is
  what prevents the double enqueue. A send **outside** a slice has no slice end, so `beam_rt` calls
  `resume_woken` itself. Both `Waiting` and `Suspended` are woken (the C's `SUSPENDED`-vs-suspend-counter
  nuance is a follow-up).
- **`ProcessRunner` is the engine seam — `beam_engine` is not a dependency.** The scheduler must not know
  how a process executes; `beam_rt`/`beam_engine` implement `ProcessRunner` over the engine's
  `ExecutionContext`, keeping the DAG acyclic (`beam_engine` → `beam_loader` only).
- **The `ProcessTable` is the ≥2-process mutation hub.** Links/monitors/signal delivery touch two PCBs;
  two `&mut Process` from a `HashMap` is a borrow-checker impossibility, so these live on `ProcessTable`
  and use a **collect-then-apply worklist**. `Process` holds only its own sets.
- **A signalled reason is COPIED into the receiver's heap at ENQUEUE time.** The source heap is dropped
  when it terminates, so embedding a reason by reference would dangle (and a safepoint GC would corrupt).
  `copy_reason_onto` `copy_struct`s it at enqueue (`enqueue_exit_signal`/`enqueue_down_signal`),
  registering referenced off-heap objects in the receiver's table; `terminate_into` copies the dying
  process's reason into every peer **before** `drop`ping its PCB; a self-signal is already in the
  receiver's heap. Immediates/literals are shared by `copy_struct`, so the common reasons pay nothing.
- **GC root set = registers + mailbox payloads + dictionary + saved args + pending signals**, gathered
  into a flat `&mut [Eterm]` (the collector takes a flat slice; `beam_gc` delegates on-heap message
  queues to `beam_process`). `Process::collect` gathers in a fixed order — registers/stack, mailbox
  payloads, dictionary, saved args, pending signals — runs the collector, then writes each region back in
  the same order: since `beam_message::Message` has no payload setter, the queue is **drained**, each
  payload appended to the root buffer, and each `Message` **rebuilt** from its relocated payload via the
  public builder; dict/saved-args/signal regions scatter back through their own `scatter_roots`. MSO
  handles are off-heap and do not move. `Process::port_monitors` is **not** a root — a `PortWatch` holds
  plain values only.
- **The dictionary compares keys — and `get_keys/1` values — with `beam_term::compare::eq` (the C's
  `EQ`).** `Dict` carries the atom table as an `Rc<AtomTable>` handle (`new()` ⇒ an empty table, whose
  unknown-name fallback is the raw atom index — the C's `EQ` for two immediates). Raw word equality would
  report a *boxed* key absent when the looked-up term is a distinct-but-equal heap word
  (`put({a},1), get({a})` ⇒ `undefined` instead of `1`). The `Rc` (not `&AtomTable`) keeps signatures
  unchanged and matches `beam_rt`'s `CodeServer` convention — no dependency edge.
- **The dictionary's iteration order is a documented deviation.** `Dict` keeps insertion order; the C
  walks its hash array (`pd_hash_get_all`), a slot-layout artifact (`put(a,1), put(b,2), get()` ⇒
  `[{b,2},{a,1}]` on OTP 29.1.1). Porting the hash array is the deferred O(1)-lookup optimization. The
  *value* semantics (replace in place / last put wins / returns the replaced value, `erase` returns the
  erased value) are pinned by tests.
- **Monitor references are on-heap `Ref` things built locally (`monitor.rs`).** `beam_term` has no
  `make_ref` in M1, so a reference is the frozen 4-word thing
  `[make_header(3, REF_SUBTAG), node(0), serial, id]` built with `make_header`;
  `beam_message::layout::object_words` copies it verbatim (no navigable children) and `compare` compares
  internal refs by raw payload words, so a `{'DOWN', Ref, …}` round-trips. When `beam_term` gains a
  proper `make_ref`, only `MonitorRef::to_eterm` changes.
- **Named monitors use a side table, not a `Monitor` field.** `erlang:monitor(process, Name)` must make
  the DOWN target the 2-tuple `{Name, node}` rather than a pid, but `Monitor`/`MonitoredBy` are built by
  struct literal in `src/process.rs` and carry only `ProcessId`s. `ProcessTable` therefore keeps
  `named_monitors: HashMap<MonitorRef, NamedTarget>` (two atoms — no heap term, no rootset) plus
  `local_node: Option<Atom>`, and the DOWN builder branches on it. A *named port* monitor is a typed
  `unsupported` (the registry resolves names to processes only). The `noproc` DOWN is delivered eagerly
  for the reason in Known Issues.
- **The `monitor/3` user tag lives in a side table too, NOT a `Monitor` field.** `Monitor`, `MonitoredBy`
  and `PortWatch` are constructed by struct literal from `beam_rt`, so adding a field would break that
  crate's compile (the workspace must stay green) — their field sets are **frozen**. `ProcessTable`
  therefore keeps `tags: HashMap<MonitorRef, Eterm>` with `remember_tag`/`forget_tag` (`forget_tag` at
  `demonitor`/`demonitor_port`/termination/port close so it cannot leak). The tag is validated to be a
  heap-independent **immediate** before any state changes (`check_tag`), because the side table is not a
  collector rootset — a heap word stored there would dangle after a GC (the oracle accepts any term as a
  tag; this crate returns the typed `unsupported` rather than a wrong result). The DOWN builders read the
  tag from the table at build time (`tags.get(&reference).copied()`, `None` ⇒ the `'DOWN'` atom), and the
  internal `PendingSignal::Down` carries it to the deferred materialisation point.
- **`Links` is a set in M1.** Re-linking is a no-op and unlinking removes once, matching the observable
  `erlang:link/1`/`unlink/1` semantics; the C's per-link refcount (for `monitor` on links and distributed
  links) is a follow-up.
- **`trap_exit` is a PCB flag, not a queue.** `exit_policy` reads it at *service* time and decides
  `Terminate`/`Trap`/`Ignore`; `Trap` builds `{'EXIT', From, Reason}` into the mailbox — exactly the
  observable `process_flag(trap_exit, true)` behaviour.
- **`Process` owns a `beam_gc::Collector`, not raw `Heap`s.** The young space is the mutator heap; policy
  (`should_collect`/`collect`) is `beam_gc::HeapPolicy`'s. No heap logic is reimplemented here.
- **`parking_lot` is the synchronisation primitive; `libc` remains unused** (declared; reserved for the
  deferred OS-level threading work).
- **The SMP pool reuses the frozen slice and bounds the table lock.** `Worker::run` calls the same
  `pub(crate)` helpers via `run_locked_quanta`, so a process executes identically under either front —
  the only difference is the worker's requeue closure, which **buffers** requeues locally. The pool holds
  one `Mutex<ProcessTable>` for at most **one reduction quantum** (`ceil(reductions_per_slice /
  lock_quantum)`, default 256), releasing it between quanta, so an independent process / cross-thread
  send / timer / dirty completion is serviced *inside* another process's slice; `lock_quantum >=
  reductions_per_slice` reproduces the old one-lock-hold behaviour exactly. Exactly one process
  **executes** at a time: it keeps its whole budget and stays in-flight (out of every run queue), and each
  quantum re-checks `table.contains(pid)` since a peer may terminate it between quanta. **Locking
  discipline (BINDING):** three families (queue locks, the table lock, the signals mutex) under the order
  in `src/smp/CONTEXT.md` — **never hold a queue lock while taking the table lock and vice-versa**;
  requeues produced while the table guard is held are buffered and flushed **only after** the guard drops;
  the only nested acquisition is *signals mutex → queue lock*.
- **`SharedProcessTable`'s `unsafe impl Send/Sync` is a contained handoff.** `ProcessTable` is `!Send`;
  the two impls assert that confining every access to the inner `parking_lot::Mutex` makes the wrapper
  sound. Measured `!Send` causes: `beam_alloc::Heap`'s `base: *mut Eterm` (via
  `beam_gc::HeapSpace`/`Collector`), `beam_gc::MsoHandle`'s `ptr: NonNull<Mso>` (via
  `beam_message::OffHeapTable`), and this crate's own `Option<Box<dyn DistSignalRouter>>` (fixable here
  with `: Send`, but insufficient alone). `beam_alloc::RefcBinary` is already `Send`. The durable fix
  belongs upstream — do not widen this `unsafe`.
- **Non-flaky `wait_idle` and loss-free park/notify.** The pool reports idle only when every queue is
  empty **and** `InFlight` is zero; a pop increments `InFlight` in the same critical section that
  dequeues, and a worker flushes its local `buf` **before** `finish_slice` decrements (flushing after
  would leave the queues empty and the count zero while a yielded pid sits only in `buf`). A worker
  re-checks the queue predicate **under the signals mutex** immediately before `Condvar::wait` and every
  notifier takes that mutex before signalling, so a wake-up cannot be lost; the mutex→queue-lock order is
  consistent everywhere.
## Known Issues (intentionally NOT covered in Milestone-1)
- **Multi-scheduler / SMP (remaining gaps)**: lock-free/threaded run queues, scheduler **balancing**, and
  the `+S`/`+SDcpu`/`+SDio` flag parsing are not modelled. The table lock is narrower (one reduction
  quantum, not a slice) but still exclusive, so a process **does not run in parallel** across
  schedulers (exactly one executes at a time). Dirty-job **term plumbing** is absent (a `DirtyJob`
  carries no live heap data) and a timer message must be thread-valid (no heap-resident terms).
  `ThreadProgress` implements the barrier but has **no** delayed-dealloc coupling with `beam_alloc`, and
  **port tasks are not load-balanced** across schedulers.
- **The signal queue is not cross-ordered with the mailbox.** A trapped `{'EXIT', …}`/`{'DOWN', …}`
  becomes a mailbox message only when the queued signal is *applied*, so an ordinary send already in the
  mailbox can be seen before it; remote (external-pid) and port signals bypass the queue entirely
  (materialised straight into the mailbox). The C's per-*sender* signal-queue granularity and the
  signal-vs-message ordering barrier remain follow-ups.
- **The `noproc` DOWN is delivered eagerly (documented deviation).** `monitor/2` on a non-live target
  materialises `{'DOWN', Ref, process, Target, noproc}` straight into the watcher's mailbox, so
  `process_info(self(), message_queue_len)` reports 1 immediately after the call where the oracle reports
  0 (it defers the DOWN as a signal and reveals it at the next `receive`). Deferring here would be
  *wrong* until the runtime services pending signals when entering `receive`, so eagerness is
  deliberate: the receive-visible result and the relative order against later DOWNs both match the
  oracle. Servicing `service_signals` at the receive point is what would make it exact.
- **Priority run queues**: port tasks, and the C's shared NORMAL/LOW physical queue
  `schedule_count`/`RESCHEDULE_LOW` fairness deferral (this crate uses four physically distinct FIFOs —
  see `run_queue.rs`), are follow-ups. `Scheduler::set_priority`/`SchedulerPool::set_priority` DO
  requeue an already-queued process. The `save_calls` *effect* (actually saving the last `N` calls for
  `seq_trace`/`dbg`) is not modelled — only the `process_flag(save_calls, N)` value contract.
- **Pre-emption has no producer in Milestone 1.** `ProcessRunner::run` receives a `budget` and the
  scheduler requeues on `RunOutcome::Yield`, and `tests/preemption.rs` proves a budget-respecting runner
  interleaves CPU-bound processes round-robin with no starvation — but the production runner
  (`beam_rt::RuntimeRunner`) ignores its `budget` and never yields, so a non-yielding process is not
  pre-empted. Making `RuntimeRunner` honour the budget is the adoption `beam_rt` owes.
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
- **Port-monitor cleanup on death is complete only when a `PortTable` is threaded in.** `terminate_into`
  always drains `Process::port_monitors` (and the exit observer always sees them), but the matching
  *incoming* `PortMonitor` entries are removed from the `PortTable` only when the caller supplies one —
  i.e. through `terminate_with_ports`/`service_signals_with_ports`/`port_closed`. The port-table-less
  entry points (`terminate`, `service_signals`, `apply_exit_signal`/`send_exit` (`erlang:exit/2`) and
  `node_down`) pass no ports; a process killed through one of those has its outgoing watches drained but
  leaves its incoming `PortMonitor` in place until the owner runs `service_signals_with_ports` at the
  peer's next schedule-in. A host that owns both tables should terminate through the ports-aware variants.
- **Exact exit-reason term from `beam_common::ExitReason`**: signal reasons are carried as live `Eterm`
  terms (the deliverable form), not the term-independent `beam_common::ExitReason`; the mapping between
  them belongs to the raising BIF layer (`beam_bifs`).
- **`process_info` source data.** Every item this crate can answer is now backed by a public accessor
  (`tests/process_info_source.rs` pins the mapping): `status` → `Process::status()`,
  `registered_name` → `ProcessTable::name_of(pid)`, `message_queue_len` → `Process::mailbox().len()`,
  `links` → `Process::links().iter()`, `monitors` → `Process::monitors().iter()` (+
  `port_monitors()` for the `{port, Port}` entries), `monitored_by` → `Process::monitored_by()`,
  `trap_exit` → `Process::is_trapping_exit()`, `priority` → `Process::priority()`, `reductions` →
  `Process::reductions()`. Still absent: `current_function`/`current_location` (nothing tracks the
  running MFA) and the `erlang:process_info/1` whole-dictionary view.
- **Monitor semantics not modelled.** `MonitorKind` has no `TimeOffset` variant
  (`erlang:monitor(time_offset, _)` is the oracle's `badarg`, which the BIF layer raises);
  `erlang:monitor_node/2,3` is **out of this crate's scope** — it is node-level distribution needing a
  local node identity plus an inbound `DOP_*` path, neither of which this crate owns; and a **named**
  port monitor (`monitor(port, Name)`) is a typed `unsupported` (the local name registry resolves names
  to processes only, so answering with a process monitor would be a wrong result). The noproc DOWN is
  delivered **eagerly** into the watcher's mailbox whereas the oracle defers it as a signal (see the
  bullet above). `erlang:monitor/3`'s `{tag, T}` form accepts only an **immediate** `T` (a non-immediate
  or a remote-target tag is a typed `unsupported`); the tag-less `monitor/2` form is complete.
## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/pid.rs` → `ProcessId`/`PortId` (bit-exact internal pid/port immediates) + id limits
- `src/registers.rs` → `XRegs`, `Stack`, `Registers` (the root-set containers); `MAX_REG`/`MAX_ARG`
- `src/process.rs` → `Process` (PCB) incl. `status`/`status_atom`, `with_policy_and_priority` and the `port_monitors` watch set; the `heap_and_off_heap_mut` and `heap_and_mailbox_mut` split borrows; `ProcessState`, `ProcessFlags`, `DeliverOutcome`, `Process::wake`, the priority/`save_calls` accessors, message delivery + GC entry + the dict/saved-args/signal sub-rootsets
- `src/dict.rs` → `Dict` (process dictionary: put/get/erase/get_keys + GC gather/scatter; keys and `get_keys/1` values compare structurally via `beam_term::compare::eq` through an `Rc<AtomTable>` handle)
- `src/fragments.rs` → `SavedArgs` (saved BIF/NIF argument roots + GC gather/scatter)
- `src/run_queue.rs` → `Priority` (max/high/normal/low) + `RunQueue` (four per-priority FIFOs + the LOW/NORMAL fairness-deviation note + `split_off_half` for work stealing + `reprioritize`)
- `src/scheduler.rs` → `Scheduler`, `RunOutcome`, `ProcessRunner` (the engine seam), `enqueue_with_priority`, `set_priority` (the queued-process requeue), `resume_woken`, the shared `pub(crate)` slice helpers `slice_begin`/`slice_end`/`run_one`/`drain_woken_into`, and `DEFAULT_REDUCTIONS_PER_SLICE`/`DEFAULT_LOCK_QUANTUM`
- `src/smp/` → the SMP pool (`SchedulerConfig` incl. `lock_quantum`, `SharedProcessTable`, `Queues`/`reprioritize`, `Worker`/`run_locked_quanta`, `SchedulerPool` incl. `set_priority`, `SchedulerStats`/`SchedulerStat`, `PoolSignals`) — per-file detail, the locking discipline and the bounded-quantum rule in `src/smp/CONTEXT.md`
- `src/timer.rs` → `TimerService` (one thread over a deadline min-heap), `TimerRef`, `TimerSink` (the high-level `send_after`/`cancel` service)
- `src/dirty.rs` → `DirtyPool`, `DirtyKind`, `DirtyOutcome`, `DirtyJob`, `DirtyCompletionSink` (the dirty-CPU + dirty-IO worker pools)
- `src/progress.rs` → `ThreadProgress` (the cross-scheduler barrier), `ManagedThread` (the per-thread progress slot)
- `src/signal.rs` → `SignalAtoms` (incl. `port`/`noproc`), `ExitSignal`, `ReasonClass`, `ExitPolicy`/`exit_policy`, the signal-message builders (exit + `process`/`port`/remote `DOWN`)
- `src/signal_queue.rs` → `SignalQueue` + `PendingSignal`/`PendingSignalKind` (the decoupled pending-signal queue + its GC gather/scatter)
- `src/link.rs` → `Links` (bidirectional link set)
- `src/monitor.rs` → `MonitorRef`, `MonitorKind` (+ `type_name`/`from_type_name`), `PortWatch`, `Monitor`, `MonitoredBy` (field sets frozen — no `tag` field), `Monitors`, `RefAllocator`, `payload_is_down_for`
- `src/remote.rs` → `RemotePid`, `PidTarget`, `RemoteMonitor`, `DistSignal`, `DistSignalRouter` (the distribution seam)
- `src/exit_observer.rs` → `ProcessExitObserver` + `ProcessExit` (the process-death observer seam; invoked once per death by `terminate_into`)
- `src/port.rs` → `PortTable` (id alloc + ownership/link/monitor surface, the incoming half of a port monitor), `PortMonitor`, `PortClose`
- `src/ptab.rs` → `ProcessTable` (pid alloc/lookup + name registry incl. `name_of` + the monitor hub (`monitor`/`noproc`, `demonitor`/`demonitor_flush`, `monitor_named`/`set_local_node`, `monitor_port(_term)`/`demonitor_port`, and the `*_with_tag` variants over their `tags` side table) + signal/link hub + decoupled signal servicing (`service_signals`, `service_signals_with_ports`) + termination (`terminate`, `terminate_with_ports`, the port-threaded `terminate_into` cascade) + the process-death observer seam + the wake list + `is_process_alive`/`exit_reason` + the remote records + the distribution seam), `ExitEffect`
- `src/error.rs` → `pub(crate)` error constructors incl. the typed-`unsupported` raise constructors (no new error type)
- `tests/*.rs` (25 files) → the integration evidence; the authoritative per-file list is in `tests/CONTEXT.md` (`lock_scope.rs` = the bounded-quantum-lock + 1-vs-4-scheduler determinism evidence; `dict_oracle.rs`/`registry_monitors.rs`/`monitor_tag.rs`/`port_monitor_cleanup.rs` = `erl`-cross-checked semantics; `preemption.rs` = the measured fairness evidence; `capability_inventory.rs` = the typed-error inventory; `split_borrow.rs` = the two split-borrow evidence sets (`heap_and_off_heap_mut` + `Fragments::move_into`; `heap_and_mailbox_mut` + `send_fragmented`); `process_exit_observer.rs`/`port_monitor_cleanup.rs` = the death-side observer + port-monitor cleanup evidence)
- `tests/CONTEXT.md` → per-file topic list, the anti-flakiness constraint, the gate command
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
  `src/smp/CONTEXT.md` (never hold a queue lock while taking the table lock and vice-versa; buffer
  requeues while the table guard is held; the only nested order is *signals mutex → queue lock*). It
  reuses the frozen `slice_begin`/`slice_end`/`drain_woken_into` helpers (in `run_locked_quanta`) — do
  **not** re-implement slice logic in a worker.
- `ProcessTable` is `!Send`, so moving it into the pool needs `SharedProcessTable`'s two
  `unsafe impl Send/Sync` — the **contained workaround** (all access is serialised by the inner mutex).
  The measured `!Send` causes are `beam_alloc::Heap`'s `base: *mut Eterm`, `beam_gc::MsoHandle`'s
  `ptr: NonNull<Mso>` (via `beam_message::OffHeapTable`) and this crate's own
  `Option<Box<dyn DistSignalRouter>>`; `beam_alloc::RefcBinary` is already `Send`. Removing it needs an
  upstream `beam_gc`/`beam_alloc` change this crate must not make — do not widen this `unsafe`.
- Build/test with the manifest path: `erts_rust/scripts/cargo.sh test --manifest-path
  erts_rust/Cargo.toml -p beam_process` (sandbox `$HOME` is read-only — always the wrapper). Never
  `cd`; never touch `../../tools/`.
- The crate has **two** split borrows, each destructuring `self` into two disjoint `&mut` fields in safe
  Rust (no raw pointer): `heap_and_off_heap_mut()` (heap + off-heap table) for
  `beam_message::Fragments::move_into`, and `heap_and_mailbox_mut()` (heap + mailbox) for
  `beam_message::send_fragmented`. Do not add a combined raw-pointer accessor for any pair; the other
  `*_mut` pairs were audited and have no caller need.
- `beam_alloc::Heap` exhaustion (`AllocKind::Heap`) from the young heap means **collect and retry** (see
  `beam_alloc`/`beam_gc` Design Decisions), not fatal OOM — `Process::deliver` implements that retry.
- The collector moves live terms, so **every** in-heap reference must be a root. `Process::collect`
  scans registers/stack, mailbox payloads, the process dictionary (`Dict`), the saved args (`SavedArgs`)
  and the pending-signal queue (`SignalQueue`) — add any NEW in-heap sub-rootset to its gather/scatter,
  in the same order, in the same change. `Process::port_monitors` is deliberately **not** a root: a
  `PortWatch` holds only plain values (a `MonitorRef` and a `PortId`).
- **Pre-emption is the RUNNER's obligation.** A `ProcessRunner` must count its own reductions and return
  `RunOutcome::Yield` at its `budget`; the scheduler then requeues at the PCB's current priority. A
  runner that never yields is not pre-empted. `tests/preemption.rs` pins the scheduler half.
- **Oracle-probed semantics live in the tests, not this file.** Before changing monitor/registration/
  signal behaviour, run the relevant probe: `erl -noshell -eval '<expr>, halt(0).'` — the oracle is OTP
  29.1.1 / erts-17.1. The `erl`-cross-checked suites (`tests/dict_oracle.rs`, `registry_monitors.rs`)
  skip loudly when `erl` is absent.
- **Adoption owed by `beam_rt`/`beam_bifs`** (do NOT edit them from here): (1) `RuntimeRunner::run` must
  honour `budget` and return `Yield`; (2) use `ProcessState::status_atom`/`Process::status` instead of
  its own status mapping; (3) `erlang:demonitor/2 [flush]` → `demonitor_flush`; (4) `erlang:monitor/2`
  type dispatch via `MonitorKind::from_type_name` + `monitor_port_term`/`monitor_named`, and install
  `set_local_node` at boot for name targets; (5) `process_info(P, registered_name)` → `name_of`;
  `process_info(P, monitors)` must surface `Process::port_monitors()` as `{port, Port}`;
  (6) `spawn_opt` heap knobs → `beam_gc::HeapPolicy::from_spawn_opts` + `with_policy_and_priority`;
  (7) `erlang:monitor/3`'s `{tag, T}` option → the `monitor_*_with_tag` entry points (the tag is a
  `ProcessTable` side table, never a `Monitor` field — no `Monitor`/`MonitoredBy` shape change); (8) a
  **fragmented** message payload is moved onto the receiver's heap with
  `Process::heap_and_off_heap_mut()` + `beam_message::Fragments::move_into` (heap growth is still needed
  — `Collector::ensure_capacity` — for a payload larger than the M1 heap); (9) release the resources a
  dead pid owned by installing a `ProcessExitObserver` via `set_process_exit_observer` (the ETS tables a
  pid owns are the motivating case — key the cleanup on `exit.pid()`; copy `exit.reason()` if retained);
  (10) once a `PortTable` is kept alongside the `ProcessTable`, terminate through `terminate_with_ports`
  (and `service_signals_with_ports` at schedule-in) so a dead port-watcher's incoming `PortMonitor` is
  removed; (11) `beam_message::send_fragmented` through `Process::heap_and_mailbox_mut()` instead of
  re-composing the fragment primitives.
## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types and the wave-3 dependency position.
- `../../CONTEXT.md` → workspace contracts (§1 A.1 process semantics, §3 error strategy, §5 GC/memory model, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A/§D → process/scheduler structure and the memory model.
- `../beam_gc/CONTEXT.md` (the collector + MSO this crate drives), `../beam_message/CONTEXT.md` (the
  mailbox `Queue` + `CopyContext`), `../beam_alloc/CONTEXT.md` (the per-process `Heap`), `../beam_term/CONTEXT.md` (the pid/port immediates).
