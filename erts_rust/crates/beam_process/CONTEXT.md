# `beam_process` — Processes, Scheduler, Run Queues, Signals
## Intent
Owns the **OS-thread-level concurrency plumbing** of the Rust ERTS rewrite (Stage 5, Part II §H): the
per-process control block (PCB), the four-level run queue, the cooperative scheduler loop, thread
progress, the **exit-signal / link / monitor** machinery (the decoupled per-process signal queue plus
its signal-message builders), the process table (`ptab`) with the local name registry, the **port-id
table with its ownership/link/monitor surface**, and the local records for **remote (distributed)
links/monitors**. It is what makes Erlang **processes** real: isolation, `spawn`, links, monitors and
exit propagation — the process half of workspace `../CONTEXT.md` §1 **A.1** (process isolation / link /
monitor / exit semantics).
It is consumed by `beam_bifs` (spawn/send/receive/`erlang:process_info`/`monitor`/`link` BIFs),
`beam_io` (ports + the process side of I/O completion) and `beam_rt` (boot/CLI/`system_info`).
It **drives** `beam_gc` (collector + MSO) at safepoints and **uses** `beam_message` (`Queue` mailbox
+ `send`/`receive` copy engine) — it never reimplements either (Prevent-Duplication).
Legacy C ground truth (read-only): `../../erts/emulator/beam/erl_process.{c,h}` (PCB, scheduler, run
queues), `erl_proc_sig_queue.c` (signal queue), `erl_monitor_link.c` (links/monitors),
`erl_ptab.{c,h}` (process/port table + name registry), `erl_process_dict.c`, `erl_vm.h`
(`MAX_REG`/`MAX_ARG`).
## Milestone-1 scope (what "implemented" means for THIS pass)
A **correct SINGLE-OS-THREAD** core — one scheduler (`Scheduler`), no thread pools, no
dirty schedulers, no lock-free/threaded run queues, no thread-progress barrier beyond a documented
single-threaded stub. Multi-scheduler balancing and real `ThreadProgress` are a documented
follow-up (see Known Issues). "Correct" means: the pid table, the **four-level** run-queue
scheduling order (max/high/normal/low), message delivery, the **decoupled per-process signal
queue** (exit signals + monitor-DOWNs, applied at schedule-in or eagerly for a non-running
receiver), the process dictionary and saved-argument sub-rootsets, the **port ownership/link/monitor**
surface, **remote (distributed) links/monitors** routed through the `DistSignalRouter` seam, and a
safepoint GC over the process root set all behave per the C semantics for the common cases, and are
covered by tests.
## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_process::prelude::*`.
### `pid.rs` — process/port identity (bit-exact with `beam_term::immediate`)
- `pub struct ProcessId { number: u64, serial: u64 }` (`Copy + Clone + PartialEq + Eq + Hash +
  PartialOrd + Ord + Debug`).
  - `pub const fn new(number: u64, serial: u64) -> ProcessId`
  - `pub const fn number(self) -> u64`; `pub const fn serial(self) -> u64`
  - `pub const fn to_eterm(self) -> Eterm` (= `make_internal_pid(make_pid_data(serial, number))`)
  - `pub fn from_eterm(term: Eterm) -> Result<ProcessId>` (validates `is_internal_pid`, else `BadArg`)
  - `pub const fn is_process(term: Eterm) -> bool` (= `is_internal_pid`)
- `pub struct PortId { number: u64 }` — same shape: `new`, `number`, `to_eterm` (`make_internal_port`),
  `from_eterm`, `is_port` (`is_internal_port`).
- `pub const MAX_PROCESS_NUMBER: u64 = (1 << 28) - 1` (`PID_NUM_SIZE`); `pub const MAX_PROCESS_SERIAL: u64`
  (`2^32 - 1`, `PID_SER_SIZE`).
### `registers.rs` — the root-set containers (x-register file + Erlang stack)
Honors the crate-layer **root-set contract**: `x_reg_array[0..Live)` **plus the whole stack**, no frame
liveness maps.
- `pub const MAX_REG: usize = 1024` (`erl_vm.h` `MAX_REG`); `pub const MAX_ARG: usize = 255`.
- `pub struct XRegs { regs: [Eterm; MAX_REG], live: usize }` (boxed)
  - `new()` (all `NIL`, `live == 0`); `live() -> usize`; `set_live(n) -> Result<()>` (≤ `MAX_REG`)
  - `get(i) -> Result<Eterm>`; `set(i, v) -> Result<()>`; `as_slice() -> &[Eterm]`; `live_slice() ->
    &[Eterm]`; `live_slice_mut() -> &mut [Eterm]`
- `pub struct Stack { words: Vec<Eterm> }` — the Erlang process stack.
  - `new()`, `with_capacity(n)`, `len()`, `is_empty()`, `as_slice()`, `as_mut_slice()`, `push(Eterm)`,
    `pop() -> Result<Eterm>`, `clear()`, `truncate(n)`
- `pub struct Registers { x: XRegs, stack: Stack }`
  - `new()`; `x()/x_mut()`; `stack()/stack_mut()`
  - `gather_roots(&self, out: &mut Vec<Eterm>) -> usize` — appends `x[0..live)` then every stack word;
    returns the split point (`x_live`) so the words can be scattered back after a collection.
  - `scatter_roots(&mut self, roots: &[Eterm], x_live: usize)` — writes the (relocated) root words back
    into `x[0..live)` and the stack.
### `process.rs` — the process control block
- `pub enum ProcessState { Runnable, Running, Waiting, Suspended, Exiting, Terminated }` (`Copy + …`).
- `pub enum DeliverOutcome { Enqueued, Woke }` (`Debug + Clone + Copy + PartialEq + Eq`) — what a
  delivery did to the recipient: `Enqueued` = the recipient was not blocked; `Woke` = the recipient
  was blocked (`Waiting`/`Suspended`) and has been made `Runnable`.
  - `pub fn woke(self) -> bool` (true for `Woke`).
- `pub struct ProcessFlags` — a `u32` bitset newtype (no `bitflags` dep): `trap_exit`, `system`,
  `in_run_queue`, `garbage_collecting`, `active`, `sensitive`. `new()`, per-flag get/set (incl.
  `sensitive()`/`set_sensitive(bool)`), `bits()/from_bits()`.
- `pub struct Process` — the PCB. Owns: `pid: ProcessId`, `state: ProcessState`, `flags: ProcessFlags`,
  `priority: Priority`, `save_calls: u16`, `collector: beam_gc::Collector` (young + mature heap spaces),
  `off_heap: beam_message::OffHeapTable` (the process's MSO handles), `mailbox: beam_message::Queue`,
  `regs: Registers`, `dict: Dict` (the process dictionary), `saved_args: SavedArgs` (saved BIF/NIF
  arguments), `signals: SignalQueue` (the pending-signal queue), `reductions: u64`, `links: Links`,
  `monitors: Monitors` (outgoing), `monitored_by: Vec<MonitoredBy>` (incoming), `exit_reason:
  Option<Eterm>`.
  - `new(pid) -> Result<Process>` (default `HeapPolicy`); `with_policy(pid, policy) -> Result<Process>`
  - `pid()`, `state()`, `set_state()`, `flags()/flags_mut()`, `is_trapping_exit()/set_trap_exit(bool)`
  - `wake(&mut self) -> bool` — if `state` is `Waiting` or `Suspended`, set it to `Runnable` and return
    `true`; otherwise return `false`. The blocked-recipient wake used by resume-on-send.
  - `reductions() -> u64`, `bump_reductions(n)`, `reset_reductions()`
  - `heap(&self) -> &beam_alloc::Heap`, `heap_mut(&mut self) -> &mut beam_alloc::Heap` (the **young**
    mutator heap, `collector.young_mut().heap_mut()`)
  - `priority() -> Priority`, `set_priority(Priority)`; `save_calls() -> u16`, `set_save_calls(u16)`;
    `is_sensitive()/set_sensitive(bool)`
  - `mailbox() -> &Queue`, `mailbox_mut() -> &mut Queue`
  - `registers() -> &Registers`, `registers_mut() -> &mut Registers`
  - `dict() -> &Dict`, `dict_mut() -> &mut Dict`; `saved_args() -> &SavedArgs`, `saved_args_mut() -> &mut SavedArgs`
  - `signals() -> &SignalQueue`, `signals_mut() -> &mut SignalQueue`; `pending_signals() -> usize` (the
    queue length, for pending-signal introspection)
  - `off_heap() -> &OffHeapTable`, `off_heap_mut() -> &mut OffHeapTable`
  - `links() -> &Links`, `links_mut() -> &mut Links`
  - `monitors() -> &Monitors`, `monitors_mut() -> &mut Monitors`
  - `monitored_by() -> &[MonitoredBy]`, `monitored_by_mut() -> &mut Vec<MonitoredBy>`
  - `exit_reason() -> Option<Eterm>`, `set_exit_reason(Eterm)`
  - **message delivery:** `deliver(&mut self, from: Eterm, term: Eterm, src_off_heap: &OffHeapTable)
    -> Result<DeliverOutcome>` — copies `term` into the young heap and enqueues the `Message`
    (collect-and-retry once on `AllocKind::Heap`/`Message` exhaustion), then `wake()`s the process and
    reports `Woke`/`Enqueued`. `deliver_simple(&mut self, from: Eterm, term: Eterm) ->
    Result<DeliverOutcome>` — the no-off-heap convenience. `receive<F: FnMut(&Message)->bool>(&mut self,
    pred: F) -> Option<Message>` (delegates to `beam_message::receive`).
  - **GC:** `collector() -> &Collector`, `should_collect(need_words) -> bool`, `heap_used_words()`,
    `collect(&mut self) -> Result<Collection>` (gather the root set = registers/stack + mailbox payloads
    + the process dictionary + the saved BIF/NIF arguments + the pending signals, run the collector,
    then scatter each region back in the same order and rebuild the mailbox), `collect_if_needed(need)
    -> Result<bool>`.
### `dict.rs` — the per-process dictionary
- `pub struct Dict` (`Default`) — the process dictionary backing `erlang:put/2`/`get/1`/`erase/1`.
  - `new()`; `put(Key, Val) -> Option<Eterm>` (replace-in-place, last put wins; returns the replaced
    value); `get(Key) -> Option<Eterm>`; `erase(Key) -> Option<Eterm>`; `erase_all() -> Vec<(Eterm,Eterm)>`;
    `get_keys() -> Vec<Eterm>`; `get_keys_for_value(Val) -> Vec<Eterm>`; `len()`, `is_empty()`,
    `entries() -> impl Iterator<Item = (Eterm, Eterm)> + '_`.
  - **GC pair:** `gather_roots(&self, out: &mut Vec<Eterm>) -> usize` (appends each entry's key then
    value; returns the offset at which its words begin); `scatter_roots(&mut self, roots: &[Eterm],
    start)` (writes the relocated key/value words back; total/never-panic).
  - **Semantics:** `Option::None` is the C `THE_NON_VALUE` (absent); the BIF layer maps it to the
    `undefined` atom. Term-agnostic (no `AtomTable`): keys compare by raw `Eterm` word equality (exact
    for immediates; structural equality of distinct-but-equal boxed keys is a follow-up). Holds terms
    resident in THIS process's young heap (a `Process::collect` sub-rootset).
### `fragments.rs` — saved BIF/NIF arguments
- `pub struct SavedArgs` (`Default`) — the saved BIF/NIF argument root list (the C `ErlSaveCallArgs`).
  - `new()`; `len()`, `is_empty()`, `clear()`; `push(Eterm)`; `pop() -> Option<Eterm>`;
    `as_slice() -> &[Eterm]`; `as_mut_slice() -> &mut [Eterm]`.
  - **GC pair:** `gather_roots(&self, out: &mut Vec<Eterm>) -> usize` / `scatter_roots(&mut self, roots:
    &[Eterm], start)` — the saved-args region of `Process::collect`'s root set (after the dictionary,
    before the pending signals).
  - A real in-heap `ErlHeapFragment` type is absent from the frozen siblings (`beam_message`/`beam_gc`
    own that follow-up); these args are a plain `Eterm` root list, not a duplicate fragment type.
### `run_queue.rs` — the ready queue (four priority levels)
- `pub enum Priority { Max = 0, High = 1, Normal = 2, Low = 3 }` (`Debug + Clone + Copy + PartialEq +
  Eq + Hash + PartialOrd + Ord`; `Default = Normal`). The derived `Ord` is the C *numeric* order
  (`Max < High < Normal < Low`), which is **NOT** scheduling precedence — precedence is `LEVELS`.
  - `const LEVELS: [Priority; 4] = [Max, High, Normal, Low]` (highest-first scheduling order);
    `index() -> usize` / `from_index(u8) -> Option<Priority>` (the C `PRIORITY_*` numeric value);
    `name() -> &'static str` (`"max"|"high"|"normal"|"low"`) / `from_name(&str) -> Option<Priority>`.
- `pub struct RunQueue { queues: [VecDeque<ProcessId>; 4] }` — **four per-priority FIFOs** (one per
  `Priority`); `dequeue` serves the highest-priority non-empty level, FIFO within a level.
  - `new()`; `enqueue(pid)` (⇒ **normal**); `enqueue_with_priority(pid, Priority)`; `dequeue() ->
    Option<ProcessId>`; `dequeue_with_priority() -> Option<(ProcessId, Priority)>`; `len()`;
    `len_at(Priority)`; `is_empty()`; `contains(pid) -> bool`; `priority_of(pid) -> Option<Priority>`;
    `remove(pid) -> bool`; `iter() -> impl Iterator<Item = ProcessId> + '_` (dequeue order);
    `iter_at(Priority) -> impl Iterator<Item = ProcessId> + '_`; `clear()`.
  - **Fairness deviation (documented):** the C keeps MAX/HIGH on dedicated queues but lets **NORMAL
    and LOW share one physical queue**, deferring LOW by a `schedule_count` (`RESCHEDULE_LOW = 8`,
    `erl_process.h`). This crate uses **four physically distinct FIFOs**, so a LOW process runs only
    once MAX/HIGH/NORMAL are all empty and never interleaves with NORMAL; the relative precedence is
    unchanged (LOW still last), only the LOW/NORMAL interleaving ratio differs.
### `scheduler.rs` — the scheduler loop + the engine seam
- `pub enum RunOutcome { Yield, Wait, Suspend, Exit(Eterm) }` — what happened in a reduction slice
  (`Exit(reason_term)` = the process terminated with that reason).
- `pub trait ProcessRunner` — **the engine seam** (`beam_engine` must NOT be a dependency of this
  crate; `beam_rt`/`beam_engine` implement this over an `ExecutionContext`):
  - `fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, budget: u32) -> Result<RunOutcome>`
    — run process `pid` for up to `budget` reductions, mutating it (and other processes, via
    `table`) as needed.
- `pub struct Scheduler { run_queue: RunQueue, reductions_per_slice: u32, progress: ThreadProgress,
  slices: u64 }`
  - `new()` (default `DEFAULT_REDUCTIONS_PER_SLICE = 4000`); `with_slice(u32)`;
    `reductions_per_slice()`; `run_queue()/run_queue_mut()`; `enqueue(pid)` (⇒ **normal**);
    `enqueue_with_priority(pid, Priority)`; `progress()/progress_mut()`
  - `run_slice<R: ProcessRunner>(&mut self, table: &mut ProcessTable, runner: &mut R) ->
    Result<Option<ProcessId>>` — pop one pid, **service its pending signals** (see below), then run it
    for one slice; apply the outcome: `Yield` ⇒ requeue at the tail of the process's **own** current
    priority; `Wait`/`Suspend` ⇒ leave the run queue (state set by the runner); `Exit(reason)` ⇒
    `table.terminate(pid, reason)` (propagation happens there). **After the outcome** it absorbs the
    table's wake list via `resume_woken` (and also on the empty-queue early return), so a send during
    the slice that woke a blocked receiver is turned into a run-queue entry in the same sweep.
    **Schedule-in signal servicing:** before `runner.run` it calls `table.service_signals(pid)`, where
    a deferred exit signal finally lands; servicing a fatal signal can terminate the process, in which
    case `run_slice` returns early (the pid ran zero reductions).
  - `resume_woken(&mut self, table: &mut ProcessTable) -> usize` — drain `table.drain_woken()` and,
    for each pid that is a still-live `Runnable` process not already in a run queue, set
    `in_run_queue(true)` and enqueue it **at its own priority**; return the number enqueued. Also the
    hook `beam_rt` calls after a send made **outside** a slice (a woken-then-terminated/re-blocked
    process is skipped; the `Yield` branch already requeued the process that just ran, so its stale
    wake is not double-counted).
  - `run_ready<R: ProcessRunner>(&mut self, table: &mut ProcessTable, runner: &mut R, max_slices:
    Option<u64>) -> Result<u64>` — run until the ready queue is empty (or `max_slices` slices run);
    returns the number of slices executed.

### The single-threaded orchestration contract `beam_rt` must follow
(`beam_rt` is the orchestrator; `Scheduler::resume_woken` is the only coupling.)
- **spawn** ⇒ `let pid = table.spawn_default()?; scheduler.enqueue(pid);`
- **a runner that blocks on an empty receive** must set the process state to `ProcessState::Waiting`
  (via `table.get_mut(pid)`) itself **before** returning `RunOutcome::Wait` — the scheduler does not
  guess the transition.
- **send from inside a runner** ⇒ `table.deliver(_simple)(to, from, term)`; a send that wakes a
  blocked receiver is absorbed by the scheduler at the end of the slice (nothing else to do).
- **send from outside a slice** ⇒ after `table.deliver(_simple)(…)` call
  `scheduler.resume_woken(&mut table)` so the woken receiver is enqueued.
- **run** ⇒ `scheduler.run_ready(&mut table, &mut runner, None)`.
### `progress.rs` — thread progress (documented single-scheduler stub)
- `pub struct ThreadProgress` — M1 no-op barrier under one scheduler: `new()`, `generation() ->
  u64`, `bump()` (advances the generation), `wait(u64)` (returns immediately — no other schedulers
  exist). The seam `beam_io`/`beam_bifs` need for "wait until all schedulers have seen generation N";
  the real cross-scheduler barrier is a documented follow-up.
### `signal.rs` — exit signals, reasons and signal-message builders
- `pub struct SignalAtoms { exit: Atom, down: Atom, process: Atom, port: Atom, normal: Atom, kill:
  Atom, killed: Atom }` — the well-known atom indices used to build signal messages.
  - `from_table(t: &mut beam_term::AtomTable) -> SignalAtoms` (interns `'EXIT'`, `'DOWN'`, `process`,
    `port`, `normal`, `kill`, `killed`); accessors incl. `port()`; `normal_term()/kill_term()/
    killed_term() -> Eterm`.
  - `classify(&self, reason: Eterm) -> ReasonClass` — `Normal | Kill | Other` by comparing `reason` to
    the well-known atoms.
- `pub enum ReasonClass { Normal, Kill, Other }`
- `pub struct ExitSignal { pub from: ProcessId, pub reason: Eterm }` (`Copy`).
- `pub enum ExitPolicy { Terminate(Eterm), Trap, Ignore }` — `Trap` = deliver `{'EXIT', From, Reason}`
  as a message.
- `pub fn exit_policy(reason: Eterm, trapping: bool, atoms: &SignalAtoms) -> ExitPolicy` — the
  propagation rule: `kill` ⇒ `Terminate(killed)` (untrappable); else trapping ⇒ `Trap`; else `normal`
  ⇒ `Ignore`; else `Terminate(reason)`.
- **Signal-message builders** (each allocates on the *receiving* process's heap):
  - `build_exit_message(heap, atoms, from: ProcessId, reason) -> Result<Eterm>` — `{'EXIT', From,
    Reason}`; `build_exit_message_term(heap, atoms, from: Eterm, reason)` — the general form, whose
    `from` may be an external pid term.
  - `build_down_message(heap, atoms, reference, pid: ProcessId, reason) -> Result<Eterm>` — `{'DOWN',
    Reference, process, Pid, Reason}`; `build_down_message_port(heap, atoms, reference, port: PortId,
    reason)` — the `port`-tagged form; `build_down_message_remote(heap, atoms, reference, remote:
    RemotePid, reason)` — materialises the external-pid target on the same heap.
  - `build_down_message_term(heap, atoms, reference, tag: Atom, target: Eterm, reason)` — the fully
    general `{'DOWN', Reference, Tag, Target, Reason}` form.
### `signal_queue.rs` — the decoupled per-process pending-signal queue
A signal is *queued* on the receiver and applied later (see the eager-vs-queued rule under `ptab.rs`);
its `reason` is copied into the receiver's own heap at enqueue time, so the queue is an in-heap
sub-rootset.
- `pub enum PendingSignalKind { Exit, Down }`.
- `pub enum PendingSignal { Exit { from: ProcessId, reason: Eterm }, Down { reference: MonitorRef,
  target: ProcessId, kind: MonitorKind, reason: Eterm } }` (`Copy`) — the queued signal.
  - `exit(from, reason) -> PendingSignal`; `down(reference, target, kind, reason) -> PendingSignal`;
    `kind() -> PendingSignalKind`; `reason() -> Eterm` (a `set_reason` is private, used by `scatter`).
- `pub struct SignalQueue { entries: VecDeque<PendingSignal> }` (`Default`) — the per-process FIFO.
  - `new()`, `push(PendingSignal)` (back), `pop() -> Option<PendingSignal>` (front), `peek() ->
    Option<&PendingSignal>`, `drain() -> Vec<PendingSignal>`, `clear()`, `len()`, `is_empty()`,
    `iter() -> impl Iterator<Item = &PendingSignal> + '_` (application order).
  - **GC pair:** `gather_roots(&self, out: &mut Vec<Eterm>) -> usize` (one word per queued signal's
    `reason`; returns the start offset) / `scatter_roots(&mut self, roots: &[Eterm], start)` (writes
    the relocated reason words back; total/never-panic). Monitor references are stored by value, not on
    the heap.
  - **Ordering:** a total FIFO order per receiver (a single FIFO) — which subsumes the C's
    per-`(sender, receiver)` order; **not** cross-ordered with the mailbox.
### `link.rs` — bidirectional links
- `pub struct Links { pids: Vec<ProcessId> }` — `new()`, `len()`, `is_empty()`, `add(pid) -> bool`
  (false if already linked), `remove(pid) -> bool`, `contains(pid) -> bool`, `iter()`, `clear()`,
  `drain() -> Vec<ProcessId>`.
### `monitor.rs` — monitors and monitor references
- `pub struct MonitorRef { id: u64 }` (`Copy + Clone + PartialEq + Eq + Hash + Debug`) — `id() ->
  u64`; `to_eterm(self, heap) -> Result<Eterm>` (builds the on-heap `Ref` thing, see Design Decisions).
- `pub enum MonitorKind { Process, Port }`.
- `pub struct Monitor { pub reference: MonitorRef, pub target: ProcessId, pub kind: MonitorKind }` — an outgoing monitor.
- `pub struct MonitoredBy { pub reference: MonitorRef, pub watcher: ProcessId, pub kind: MonitorKind }` — an incoming monitor.
- `pub struct Monitors { entries: Vec<Monitor> }` — `new()`, `len()`, `is_empty()`, `push(Monitor)`,
  `remove(reference) -> Option<Monitor>`, `iter()`, `find(reference) -> Option<&Monitor>`, `drain()`.
- `pub struct RefAllocator { next: u64 }` — `new()`, `next() -> MonitorRef` (monotonic, never reuses an
  id). Owned by `ProcessTable`.
### `ptab.rs` — the process/port table + name registry + signal/link/monitor hub
The single place that mutates ≥2 processes (mirrors the C ptab lock domain); M1 single-threaded, so it
holds `Process` values directly. (`PortTable` now lives in `port.rs`.)
- `pub struct ProcessTable { slots: Vec<Slot>, free: Vec<u64>, names: HashMap<Atom, ProcessId>,
  refs: RefAllocator, atoms: SignalAtoms, ready: Vec<ProcessId>, remote_links: HashMap<ProcessId,
  Vec<RemotePid>>, remote_monitors: HashMap<ProcessId, Vec<RemoteMonitor>>, router: Option<Box<dyn
  DistSignalRouter>> }` (`Slot { serial: u64, process: Option<Process> }`). `ready` is the **wake
  list**: pids of blocked processes a delivery/signal has just made runnable; `router` is the
  distribution seam (`None` ⇒ record-only).
  - `new(atoms: SignalAtoms) -> ProcessTable`
  - **allocation / lookup:** `alloc_pid() -> Result<ProcessId>` (reuses a free slot, bumping its
    serial; `SystemLimit` when the number space is exhausted); `spawn(policy: HeapPolicy)`;
    `spawn_default()`; `insert(Process) -> Result<()>`; `get(pid) -> Option<&Process>`; `get_mut(pid)`;
    `take(pid) -> Option<Process>` (bumps the slot serial **and drops the pid from the wake list**);
    `remove(pid)`; `contains(pid) -> bool`; `len()`; `pids() -> impl Iterator<Item = ProcessId> + '_`.
  - **priority:** `set_priority(pid, priority: Priority) -> Result<()>` — delegates to
    `Process::set_priority`; unknown pid ⇒ `TermNotFound`.
  - **message delivery:** `deliver(to: ProcessId, from: Eterm, term: Eterm, src_off_heap:
    &OffHeapTable) -> Result<DeliverOutcome>` — the table-level send: calls the target's
    `Process::deliver` and, on `Woke`, pushes `to` onto the wake list (unknown `to` ⇒ `TermNotFound`).
    `deliver_simple(to, from, term) -> Result<DeliverOutcome>`; `drain_woken() -> Vec<ProcessId>`
    (return **and** clear the wake list); `woken() -> &[ProcessId]`.
  - **signals / links / monitors (local pids):** `apply_exit_signal(target, signal: ExitSignal) ->
    Result<ExitEffect>`; `link(a, b) -> Result<()>`; `unlink(a, b) -> Result<()>`; `send_exit(from, to,
    reason: Eterm)` (the `exit/2` operation); `monitor(watcher, target, kind: MonitorKind) ->
    Result<MonitorRef>`; `demonitor(watcher, reference) -> Result<bool>`; `terminate(pid, reason)`.
  - **decoupled signal servicing:** `service_signals(pid) -> Result<()>` — drains + applies `pid`'s
    pending-signal queue in FIFO order, cascading to any process those signals affect (the scheduler
    calls it at schedule-in); `pending_signals(pid) -> Result<usize>`; `signals(pid) ->
    Result<&SignalQueue>`.
  - **arbitrary pid terms (local or remote):** `link_term(a, b: Eterm)`, `unlink_term(a, b: Eterm)`,
    `monitor_term(watcher, target: Eterm, kind) -> Result<MonitorRef>`, `send_exit_term(from, to:
    Eterm, reason)` — a local pid routes to the local op, an external pid is recorded + routed (see
    `remote.rs`). `remote_links(pid) -> &[RemotePid]`; `remote_monitors(pid) -> &[RemoteMonitor]`.
  - **distribution seam:** `set_dist_router(Box<dyn DistSignalRouter>)`; `take_dist_router() ->
    Option<Box<dyn DistSignalRouter>>`; `has_dist_router() -> bool`.
  - **whole-node down / port close:** `node_down(node: Eterm, reason)` — resolves + purges every remote
    relationship with `node` (a `{'DOWN', …}` per monitor, a trapped/fatal `{'EXIT', …}` per link);
    `port_closed(ports: &mut PortTable, port: PortId, reason)` — drains the port's owner/links/monitors
    and delivers the `{'DOWN', Ref, port, Port, Reason}` / `{'EXIT', Port, Reason}` shapes.
  - **name registry:** `register(name: Atom, pid) -> Result<()>`; `unregister(name) -> Result<()>`;
    `whereis(name) -> Option<ProcessId>`; `registered() -> impl Iterator<Item = (Atom, ProcessId)> + '_`.
- **Eager-vs-queued rule (the signal ordering contract).** A signalling operation **enqueues** a
  `PendingSignal` on the receiver (copying the reason into the receiver's own heap) — never applies it
  inline. Servicing then happens (a) at the receiver's **schedule-in** (`service_signals`, called by
  `run_slice`) and (b) **eagerly at the end of the sending operation** for a receiver that is *not* the
  currently `Running` process (ERTS's early signal handling): `apply_exit_signal`/`terminate`/`send_exit`
  share one rule — `schedule_service` adds every receiver to the service worklist unless it is
  `ProcessState::Running`. `apply_exit_signal` returns the `exit_policy`-predicted `ExitEffect` for a
  merely-queued signal (the observed effect for an eagerly-applied one). A **remote** (external-pid) or
  **port** signal does not go through the queue at all: it carries no `trap_exit` policy to defer, so
  `node_down`/`port_closed` materialise it straight into the mailbox.
- `pub enum ExitEffect { Terminated, Trapped, Ignored }` — the outcome of `apply_exit_signal`
  (whether the target was terminated, got a `{'EXIT'}` message, or ignored the signal).
### `remote.rs` — remote (external-node) links and monitors
The **local record** of a process's relationships with processes on other nodes. The wire format is
`beam_dist`'s, reached through a trait seam (this crate never depends on `beam_dist`).
- `pub struct RemotePid { node: Eterm, number: u32, serial: u32, creation: u32 }` (`Copy + …`) — a
  decomposed external pid (the `[header, node, creation, data]` thing), holding no boxed pointer into
  another heap.
  - `from_eterm(term: Eterm) -> Result<RemotePid>` (non-external-pid ⇒ `BadArg`); `node() ->
    Eterm`; `number()/serial()/creation() -> u32`; `to_eterm(self, heap: &mut Heap) -> Result<Eterm>`
    (materialises the thing again).
- `pub enum PidTarget { Local(ProcessId), Remote(RemotePid) }` — `from_eterm(term: Eterm) ->
  Result<PidTarget>` (external ⇒ `Remote`, internal ⇒ `Local`, anything else ⇒ `BadArg`).
- `pub struct RemoteMonitor { pub reference: MonitorRef, pub remote: RemotePid, pub kind: MonitorKind }`
  — an outgoing monitor on a remote process/port.
- `pub enum DistSignal { Link { from, to }, Unlink { from, to }, Monitor { watcher, target, reference,
  kind }, Demonitor { watcher, target, reference }, Exit { from, to, reason } }` — the control signal
  the runtime must route to the remote node (the `DOP_*` mapping is documented on the module).
- `pub trait DistSignalRouter { fn route(&mut self, signal: DistSignal) -> Result<()>; }` — **the
  distribution seam**: implemented downstream (`beam_rt`) over `beam_dist`. `route` must serialise the
  signal **synchronously** (`reason` may live in the signaller's heap, which the caller may drop right
  after). The table's remote records (`remote_links`/`remote_monitors`) and `set_dist_router`/
  `has_dist_router` are on `ptab.rs`.
### `port.rs` — the port-id table + ownership / link / monitor surface
`PortTable` is the number table the C keeps alongside the pid table, extended with the relationships a
dying port must resolve. Port *behaviour* (driver, scheduling, read path) is `beam_io`'s. The crate-root
re-export keeps `beam_process::PortTable` resolving here.
- `pub struct PortMonitor { pub reference: MonitorRef, pub watcher: ProcessId }` — a monitor placed on
  a port.
- `pub struct PortClose { pub owner: Option<ProcessId>, pub links: Vec<ProcessId>, pub monitors:
  Vec<PortMonitor> }` — what `close` releases.
- `pub struct PortTable { slots: Vec<bool>, free: Vec<u64>, owners, links, monitors }` —
  - `new()`; `alloc() -> Result<PortId>` (`SystemLimit` when the number space is exhausted);
    `alloc_owned(owner: ProcessId) -> Result<PortId>`; `release(PortId)` (frees the number and drops
    every relationship); `close(PortId) -> Result<PortClose>` (frees the number, returns **and clears**
    owner/links/monitors; unknown id ⇒ `BadArg`); `contains(PortId) -> bool`; `len()`.
  - **ownership:** `owner(PortId) -> Option<ProcessId>`; `set_owner(PortId, ProcessId) -> Result<()>`
    (unknown id ⇒ `BadArg`); `clear_owner(PortId)`.
  - **links:** `link(PortId, ProcessId) -> Result<bool>` (deduped — `false` if already linked);
    `unlink(PortId, ProcessId) -> Result<bool>`; `links(PortId) -> &[ProcessId]`.
  - **monitors:** `monitor(PortId, watcher: ProcessId, reference: MonitorRef) -> Result<()>`;
    `demonitor(PortId, reference: MonitorRef) -> Result<bool>`; `monitors(PortId) -> &[PortMonitor]`.
  - Every relationship mutation on an unallocated id returns `BeamError::BadArg`.
    `ProcessTable::port_closed` is the one-call orchestrator resolving a closing port's relationships.
### `error.rs` — `pub(crate)` constructors only (no new error enum)
## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only. Never a crate-local error enum, never `panic!`
  on runtime data, never `unwrap()` on runtime data. Unknown pid/name/ref ⇒
  `BeamError::TermNotFound`; a malformed pid term or out-of-range register index ⇒ `BeamError::BadArg`;
  pid/port space exhaustion ⇒ `BeamError::SystemLimit`; heap exhaustion during delivery ⇒ propagate the
  underlying `BeamError::Alloc` after a collect-and-retry. `beam_term::TermError` is reconciled
  upstream, so `?` crosses the boundary.
- **`unsafe` is confined** to the modules that touch raw words: `signal.rs`/`monitor.rs` message
  builders and `remote.rs` (reading/writing the external-pid thing through `beam_term`'s `unsafe`
  predicates, building tuple/ref words through `beam_alloc::Heap::alloc_words`). Every
  `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`../beam_common/src/safety.rs` convention). `unsafe_op_in_unsafe_fn` stays clean. `pid.rs`,
  `registers.rs`, `run_queue.rs`, `link.rs`, `monitor.rs` set logic, `ptab.rs` bookkeeping,
  `signal_queue.rs`, `port.rs` and `progress.rs` must be safe Rust.
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
- Files target ~1000 lines; one responsibility per file. Licensing Apache-2.0.
  `src/ptab.rs` (the table + signal hub) is the only file over that figure: ~1535 lines of production
  code + ~450 lines of tests (~1990 total). It deliberately stays whole because the signal/link/monitor
  servicing is tightly coupled to `ProcessTable`'s private fields (`slots`/`atoms`/`refs`/`ready`), which
  a sibling module cannot reach — a structural-debt candidate to extract if it grows further or if field
  visibility is widened.
## Test Strategy
- In-module `#[cfg(test)] mod tests` per module: `pid` (encode/decode round-trip vs `beam_term`
  accessors, boundary fields, `from_eterm` rejects a non-pid), `registers` (live bounds, get/set range
  errors, stack push/pop, `gather_roots`/`scatter_roots` round-trip incl. the split point),
  `run_queue` (FIFO order, the four-level priority selection + `contains`/`remove`/`priority_of`),
  `link` (`add` idempotence, `remove`, `drain`),
  `monitor` (`RefAllocator` monotonic, `Monitors` push/remove/find, `MonitorRef::to_eterm` shape),
  `signal` (`exit_policy` table: normal/kill/other × trapping/not; the exit / `process`-`DOWN` /
  `port`-`DOWN` builders' arity + head/tag atoms), `signal_queue` (FIFO ordering, drain/clear,
  introspection, the `gather_roots`/`scatter_roots` round-trip), `port` (alloc/reuse + the
  ownership/link/monitor round-trips, `close` draining every relationship), `progress` (generation
  bump), `ptab` (alloc/remove/reuse + serial bump, `register`/`whereis`/`unregister`, stale-pid
  rejection, `deliver`/`deliver_simple` `Enqueued`-vs-`Woke` + the wake list, `take` clearing a pending
  wake, and the trap/link-`EXIT`/`{'DOWN'}` signal paths waking a blocked recipient).
- **`tests/` — the required integration evidence** (these pin the M1 guarantees named in the
  objective):
  1. `tests/scheduler.rs` — **pid allocation**: many spawns get distinct pids; a freed pid number is
     reused with a **bumped serial**; a stale pid no longer resolves. **Spawn + run-queue scheduling
     order**: a `ProcessRunner` that records execution order shows strict round-robin (each ready
     process runs one slice, in enqueue order, requeued at the tail on `Yield`); a `Wait` process
     leaves the queue and is not rerun. **Resume-on-send**: a `ResumeRunner` shows a send to a blocked
     (`Waiting`) receiver wakes it and the same slice's sweep re-enqueues it, so `a` runs a second time
     and consumes the message; and a two-process counter **ping-pong** driven to a bound shows the
     chain only continues because each forward send wakes its blocked peer, terminating with the
     correct final counter.
  2. `tests/messaging.rs` — build a term in a sender's heap, `send` it into a receiver's mailbox, then
     `Process::receive` returns a single matching message and leaves non-matching ones queued (FIFO);
     and delivering to a blocked process returns `DeliverOutcome::Woke` and flips it to `Runnable`
     while a runnable process returns `Enqueued`.
  3. `tests/signals.rs` — **link propagation**: `A` links `B`, `A` terminates with a non-normal reason
     ⇒ `B` (not trapping) terminates with the same reason; and with `trap_exit` set ⇒ `B` survives and
     its mailbox holds `{'EXIT', A, Reason}`. **Monitor down-message**: `A` monitors `B`; `B`
     terminates with reason `R` ⇒ `A`'s mailbox holds `{'DOWN', Ref, process, B, R}`. **Reason
     ownership**: with a **structured (tuple)** reason (not an immediate), the delivered reason is
     `eq` to the original *and still valid after the recipient's own `Process::collect()`* — proving it
     was copied into the recipient's heap, not left as a dangling pointer into the source's (now
     dropped) heap. Covered for the trapping-peer, cascaded A→B→C link, `{'DOWN'}`, and self-signal
     cases.
  4. `tests/gc.rs` — **safepoint GC over the process root set**: allocate live terms in the young heap,
     place one in an x-register and one on the stack, enqueue a message whose payload is another live
     term, run `Process::collect()`, and assert the x-register, the stack word and the mailbox payload
     all still reference the (relocated) live terms while unreferenced garbage is reclaimed.
  5. `tests/priority.rs` — the **four-level scheduling order** (a recording/blocking `ProcessRunner`
     shows MAX → NORMAL → LOW precedence, FIFO within a level), a process requeued at **its own**
     priority on `Yield` (so a demotion during a slice takes effect at the next requeue), a woken
     process resuming at its own priority; and the `process_flag` PCB support (`priority`,
     `sensitive`, `save_calls` round-trips).
  6. `tests/pending_signals.rs` — the **decoupled queue**: a signal for a `Running` process stays pending
     until its schedule-in (the schedule-in servicing of a fatal signal means the runner never runs),
     a signal for a non-running process is applied eagerly at the end of the send, FIFO order across
     senders, and a **structured** reason surviving the receiver's `Process::collect`.
  7. `tests/ports.rs` — the `PortTable` ownership/link/monitor round-trips and the one-call
     `ProcessTable::port_closed` orchestrator: a monitor is answered by `{'DOWN', Ref, port, Port,
     Reason}`, a **trapping** linked process receives `{'EXIT', Port, Reason}` and survives, a
     **non-trapping** one terminates with the reason, and an unknown port is rejected without effect.
  8. `tests/remote.rs` — an **external pid** term links/monitors through the same table entry points as
     a local pid, is recorded separately (never in a `Process`'s own link/monitor sets), and every
     relationship change is handed to a recording `DistSignalRouter` as the exact control signal;
     `node_down` resolves every recorded monitor/link and then purges the records.
  9. `tests/dict.rs` — the `Dict` `put`/`get`/`erase`/`get_keys` semantics on the PCB, and a
     **structured** dictionary value surviving `Process::collect`.
  10. `tests/fragments.rs` — terms saved only through `Process::saved_args` surviving `Process::collect`,
     and one collection relocating registers, stack, mailbox payloads, dictionary values and saved
     arguments **together**.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
  beam_process` and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml
  --workspace`. Always go through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never
  touch `../../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness,
  not here.
## Design Decisions
- **A decoupled, ordered per-process signal queue.** A signal is **enqueued** on the receiver's own
  `SignalQueue` at generation time — never applied inline — and applied later, at the target's
  **schedule-in** (`Scheduler::run_slice` calls `ProcessTable::service_signals(pid)` before
  `runner.run`; the scheduler returns early if servicing terminated the pid) or **eagerly at the end of
  the sending operation** when the target is *not* the currently `Running` process (ERTS's early signal
  handling for a non-running target). The reason is **copied into the receiver's own heap at enqueue
  time**, so the queued signal outlives the signaller's heap and its off-heap objects; `exit_policy` is
  evaluated at **service** time, against the receiver's `trap_exit` flag *then*. **Ordering guarantee:**
  a total FIFO order per receiver (a single FIFO), which subsumes ERTS's per-`(sender, receiver)` order;
  it is **not** cross-ordered with the mailbox — a `{'EXIT', …}`/`{'DOWN', …}` becomes a mailbox message
  only when it is *applied*, so an ordinary message sent after the signal was generated can be enqueued
  first (see `signal_queue.rs` and Known Issues). The queue is an **in-heap sub-rootset**:
  `Process::collect` gathers/scatters it (after the saved args). `apply_exit_signal` returns the
  `exit_policy`-predicted `ExitEffect` for a merely-queued (deferred) signal — its signature is unchanged.
- **Resume-on-send: a wake list on the table + absorption by the scheduler.** A delivery (or a
  signal message) to a blocked process must make it runnable, and the single-threaded scheduler must
  not exit `run_ready` while a freshly-woken process is still pending. Rather than have every caller
  re-enqueue (which would double-enqueue a process that the `Yield` branch already requeued), the
  wake is *recorded* on the `ProcessTable` (`ready: Vec<ProcessId>`): `Process::deliver` returns
  `DeliverOutcome::Woke`, the table-level `deliver`/signal builders push the pid onto `ready`, and
  `Scheduler::run_slice` drains it via `resume_woken` at the end of **every** slice (and on the
  empty-queue early return). `resume_woken` skips a pid that is no longer live, is not `Runnable`, or
  is already `in_run_queue` — the last guard is what prevents the running process (requeued by
  `Yield`) from being enqueued twice when it also appears on the wake list. A send made **outside** a
  slice has no slice end to absorb it, so `beam_rt` calls `resume_woken` itself (the documented
  contract above). Both `Waiting` and `Suspended` are woken by `Process::wake`: `Waiting` is the
  ordinary empty-receive case, and waking a `Suspended` process keeps the two blocked states
  symmetric (the message must still be seen; the suspension is lifted separately) rather than
  guessing the historical C nuance — the C's `SUSPENDED`-vs-`suspend`-counter distinction is a
  follow-up.
- **`ProcessRunner` is the engine seam — `beam_engine` is not a dependency.** The scheduler must not
  know how a process executes. `beam_rt`/`beam_engine` implement `ProcessRunner` over the engine's
  `ExecutionContext`, so this crate stays at Stage-5 and the DAG stays acyclic (`beam_engine` →
  `beam_loader` only; `beam_process` → wave-0/1/2 foundations).
- **The `ProcessTable` is the ≥2-process mutation hub.** Links/monitors/signal delivery inherently
  touch two PCBs; holding two `&mut Process` from a `HashMap` is a borrow-checker impossibility, so all
  such operations live on `ProcessTable` and use a **collect-then-apply worklist** (gather the link
  targets / watchers, then mutate each target transiently). `Process` itself holds only its own sets.
- **A signalled reason is COPIED into the receiver's heap at ENQUEUE time.** An exit/link/monitor
  reason that lives in the source process's heap (e.g. a tuple like `{badmatch, X}`) must not be
  embedded by reference into a receiver: the source heap is dropped when it terminates, which would
  leave a dangling pointer in the receiver's queued signal (and a safepoint GC would then corrupt/
  crash). `copy_reason_onto` therefore `copy_struct`s the reason into the receiver's heap **when the
  signal is enqueued** (`enqueue_exit_signal`/`enqueue_down_signal`), registering every referenced
  off-heap object in the receiver's own table, so a queued reason always outlives the signaller.
  `terminate_into` copies the dying process's reason into every peer **before** `drop`ping its PCB; a
  self-signal (`source == recipient`) is already in the receiver's heap and is not copied. Immediates
  and literal-area terms are shared by `copy_struct`, so the common reasons pay nothing. This mirrors
  `Process::deliver`'s copy-then-enqueue pattern and is what makes `{'EXIT', …}`/`{'DOWN', …}` survive
  the receiver's next collection.
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
- **`load`/`libc`/`parking_lot` are declared but unused in the M1 core** (`libc`/`parking_lot` are for
  the deferred multi-scheduler/threaded run queues); the single-threaded core uses neither. Keep the
  edges (they are the documented multi-scheduler dependencies) but do not invent threads in M1.
## Known Issues (intentionally NOT covered in Milestone-1)
- **Multi-scheduler / SMP**: no scheduler threads, thread pools, dirty schedulers, scheduler balancing,
  or lock-free run queues. `ThreadProgress` is a documented single-scheduler stub; the real
  cross-scheduler barrier (and the delayed-dealloc coupling with `beam_alloc`) is a follow-up.
- **The signal queue is not cross-ordered with the mailbox.** A trapped `{'EXIT', …}`/`{'DOWN', …}`
  becomes a mailbox message only when the queued signal is *applied*, so an ordinary send already in
  the mailbox can be seen before it; remote (external-pid) and port signals bypass the queue entirely
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
- `src/run_queue.rs` → `Priority` (max/high/normal/low) + `RunQueue` (four per-priority FIFOs + the LOW/NORMAL fairness-deviation note)
- `src/scheduler.rs` → `Scheduler`, `RunOutcome`, `ProcessRunner` (the engine seam), `enqueue_with_priority`, `resume_woken` (wake absorption), the schedule-in signal servicing
- `src/progress.rs` → `ThreadProgress` (single-scheduler stub)
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
- `../../erts/emulator/beam/erl_process.{c,h}` → PCB, scheduler, run queues (read-only ground truth)
- `../../erts/emulator/beam/erl_ptab.{c,h}` → process/port table + name registry (read-only)
- `../../erts/emulator/beam/erl_monitor_link.c` → links/monitors (read-only)
- `../../erts/emulator/beam/erl_proc_sig_queue.c` → the signal-queue ground truth the decoupled pending-signal queue reproduces (read-only)
- `../../erts/emulator/beam/erl_vm.h` → `MAX_REG`/`MAX_ARG` (read-only)
- `../beam_gc/`, `../beam_message/`, `../beam_alloc/` → the collector, mailbox/copy engine and heap this crate drives (siblings — read-only, escalate writes to `..`)
## Notes for Agents
- Read `../CONTEXT.md` (crate-layer contracts, the `Process`/`Scheduler` seam) and
  `../../CONTEXT.md` §1 A.1/§3/§5 before changing this crate. Ground truth is the legacy C listed
  above — **read it; never modify it.**
- Build/test with the manifest path: `erts_rust/scripts/cargo.sh test --manifest-path
  erts_rust/Cargo.toml -p beam_process` (sandbox `$HOME` is read-only — always the wrapper). Never
  `cd`; never touch `../../tools/`.
- `Process::heap_mut()` returning two disjoint `&mut` fields (heap + mailbox) requires destructuring
  `self` when calling `beam_message::send` — do not add a combined raw-pointer accessor.
- `beam_alloc::Heap` exhaustion (`AllocKind::Heap`) from the young heap means **collect and retry**
  (see `beam_alloc`/`beam_gc` Design Decisions), not fatal OOM — `Process::deliver` implements that
  retry.
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
