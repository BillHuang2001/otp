# `beam_process` — Processes, Scheduler, Run Queues, Signals
## Intent
Owns the **OS-thread-level concurrency plumbing** of the Rust ERTS rewrite (Stage 5, Part II §H): the
per-process control block (PCB), the run queue, the cooperative scheduler loop, thread progress, the
**exit-signal / link / monitor** machinery, and the process/port table (`ptab`) with the local name
registry. It is what makes Erlang **processes** real: isolation, `spawn`, links, monitors and exit
propagation — the process half of workspace `../CONTEXT.md` §1 **A.1** (process isolation / link /
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
follow-up (see Known Issues). "Correct" means: the pid table, run-queue scheduling order, message
delivery, exit/link/monitor propagation and a safepoint GC over the process root set all behave per
the C semantics for the common cases, and are covered by tests.
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
  `in_run_queue`, `garbage_collecting`, `active`. `new()`, per-flag get/set, `bits()/from_bits()`.
- `pub struct Process` — the PCB. Owns: `pid: ProcessId`, `state: ProcessState`, `flags: ProcessFlags`,
  `priority: Priority`, `save_calls: u16`, `collector: beam_gc::Collector` (young + mature heap spaces),
  `off_heap: beam_message::OffHeapTable` (the process's MSO handles), `mailbox: beam_message::Queue`,
  `regs: Registers`, `dict: Dict` (the process dictionary), `saved_args: SavedArgs` (saved BIF/NIF
  arguments), `reductions: u64`, `links: Links`, `monitors: Monitors` (outgoing), `monitored_by:
  Vec<MonitoredBy>` (incoming), `exit_reason: Option<Eterm>`.
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
    `collect(&mut self) -> Result<Collection>` (gather the root set = registers + mailbox payloads, run
    the collector, scatter the roots back and rebuild the mailbox), `collect_if_needed(need) ->
    Result<bool>`.
### `run_queue.rs` — the ready queue
- `pub struct RunQueue { ready: VecDeque<ProcessId> }` — a FIFO of runnable pids (M1: one normal
  priority queue; the C's max/high/normal/low queues are a documented follow-up).
  - `new()`, `enqueue(ProcessId)`, `dequeue() -> Option<ProcessId>`, `len()`, `is_empty()`,
    `contains(pid) -> bool`, `remove(pid) -> bool`, `iter() -> impl Iterator<Item = ProcessId> + '_`,
    `clear()`.
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
    `reductions_per_slice()`; `run_queue()/run_queue_mut()`; `enqueue(pid)`; `progress()/progress_mut()`
  - `run_slice<R: ProcessRunner>(&mut self, table: &mut ProcessTable, runner: &mut R) ->
    Result<Option<ProcessId>>` — pop one pid, run it for one slice, then: `Yield` ⇒ requeue at the
    tail; `Wait`/`Suspend` ⇒ leave the run queue (state set by the runner); `Exit(reason)` ⇒
    `table.terminate(pid, reason)` (propagation happens there). **After the outcome** it absorbs the
    table's wake list via `resume_woken` (and also on the empty-queue early return), so a send during
    the slice that woke a blocked receiver is turned into a run-queue entry in the same sweep.
  - `resume_woken(&mut self, table: &mut ProcessTable) -> usize` — drain `table.drain_woken()` and,
    for each pid that is a still-live `Runnable` process not already in a run queue, set
    `in_run_queue(true)` and enqueue it; return the number enqueued. Also the hook `beam_rt` calls
    after a send made **outside** a slice (a woken-then-terminated/re-blocked process is skipped; the
    `Yield` branch already requeued the process that just ran, so its stale wake is not double-counted).
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
- `pub struct SignalAtoms { exit: Atom, down: Atom, process: Atom, normal: Atom, kill: Atom,
  killed: Atom }` — the well-known atom indices used to build signal messages.
  - `from_table(t: &mut beam_term::AtomTable) -> SignalAtoms` (interns `'EXIT'`, `'DOWN'`, `process`,
    `normal`, `kill`, `killed`); accessors; `normal_term()/kill_term()/killed_term() -> Eterm`.
  - `classify(&self, reason: Eterm) -> ReasonClass` — `Normal | Kill | Other` by comparing `reason` to
    the well-known atoms.
- `pub enum ReasonClass { Normal, Kill, Other }`
- `pub struct ExitSignal { pub from: ProcessId, pub reason: Eterm }` (`Copy`).
- `pub enum ExitPolicy { Terminate(Eterm), Trap, Ignore }` — `Trap` = deliver `{'EXIT', From, Reason}`
  as a message.
- `pub fn exit_policy(reason: Eterm, trapping: bool, atoms: &SignalAtoms) -> ExitPolicy` — the
  propagation rule: `kill` ⇒ `Terminate(killed)` (untrappable); else trapping ⇒ `Trap`; else `normal`
  ⇒ `Ignore`; else `Terminate(reason)`.
- `pub fn build_exit_message(heap: &mut beam_alloc::Heap, atoms: &SignalAtoms, from: ProcessId, reason:
  Eterm) -> Result<Eterm>` — builds `{'EXIT', From, Reason}`.
- `pub fn build_down_message(heap: &mut beam_alloc::Heap, atoms: &SignalAtoms, reference: Eterm, pid:
  ProcessId, reason: Eterm) -> Result<Eterm>` — builds `{'DOWN', Reference, process, Pid, Reason}`.
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
### `ptab.rs` — the process/port table + name registry + signal propagation hub
The single place that mutates ≥2 processes (mirrors the C ptab lock domain); M1 single-threaded, so it
holds `Process` values directly.
- `pub struct ProcessTable { slots: Vec<Slot>, free: Vec<u64>, names: HashMap<Atom, ProcessId>,
  refs: RefAllocator, atoms: SignalAtoms, ready: Vec<ProcessId> }` (`Slot { serial: u64, process:
  Option<Process> }`). `ready` is the **wake list**: the pids of blocked processes a
  delivery/signal has just made runnable.
  - `new(atoms: SignalAtoms) -> ProcessTable`
  - **allocation / lookup:** `alloc_pid() -> Result<ProcessId>` (reuses a free slot, bumping its
    serial; returns `SystemLimit` when the number space is exhausted); `spawn(policy: HeapPolicy) ->
    Result<ProcessId>`; `spawn_default() -> Result<ProcessId>`; `insert(Process) -> Result<()>`;
    `get(pid) -> Option<&Process>`; `get_mut(pid) -> Option<&mut Process>`; `take(pid) ->
    Option<Process>` (bumps the slot serial **and drops the pid from the wake list**); `remove(pid) ->
    Option<Process>`; `contains(pid) -> bool`; `len()`; `pids() ->
    impl Iterator<Item = ProcessId> + '_`
  - **message delivery:** `deliver(to: ProcessId, from: Eterm, term: Eterm, src_off_heap:
    &OffHeapTable) -> Result<DeliverOutcome>` — the table-level send: calls the target's
    `Process::deliver` and, on `Woke`, pushes `to` onto the wake list (unknown `to` ⇒ `TermNotFound`).
    `deliver_simple(to, from, term) -> Result<DeliverOutcome>` — the no-off-heap form.
    `drain_woken() -> Vec<ProcessId>` (return **and** clear the wake list); `woken() -> &[ProcessId]`
    (read accessor).
  - **signals / links / monitors:** `apply_exit_signal(target, signal: ExitSignal) ->
    Result<ExitEffect>`; `link(a, b) -> Result<()>`; `unlink(a, b) -> Result<()>`; `send_exit(from,
    to, reason: Eterm) -> Result<()>` (the `exit/2` operation); `monitor(watcher, target, kind) ->
    Result<MonitorRef>`; `demonitor(watcher, reference) -> Result<bool>`; `terminate(pid, reason:
    Eterm) -> Result<()>` (propagates to links + monitors, cascading through a worklist). Every signal
    path that queues a message on a recipient — the `ExitPolicy::Trap` branch of `apply_exit_signal`,
    `enqueue_exit_message` (link propagation) and `enqueue_down_message` (monitor `{'DOWN'}`) — also
    calls `Process::wake` and pushes the recipient onto the wake list, so a blocked process resumes
    when its signal message arrives (the `Terminate` branch needs no wake: `take` drops its stale wake).
  - **name registry:** `register(name: Atom, pid) -> Result<()>`; `unregister(name) -> Result<()>`;
    `whereis(name) -> Option<ProcessId>`; `registered() -> impl Iterator<Item = (Atom, ProcessId)> + '_`.
- `pub enum ExitEffect { Terminated, Trapped, Ignored }` — the outcome of `apply_exit_signal`
  (whether the target was terminated, got a `{'EXIT'}` message, or ignored the signal).
- `pub struct PortTable { ... }` — minimal port id allocation/ownership mirroring the pid table:
  `new()`, `alloc() -> Result<PortId>`, `release(PortId)`, `contains(PortId) -> bool`, `len()`. The
  port *behaviour* is `beam_io`'s; this is only the id table the C keeps alongside pids.
### `error.rs` — `pub(crate)` constructors only (no new error enum)
## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only. Never a crate-local error enum, never `panic!`
  on runtime data, never `unwrap()` on runtime data. Unknown pid/name/ref ⇒
  `BeamError::TermNotFound`; a malformed pid term or out-of-range register index ⇒ `BeamError::BadArg`;
  pid/port space exhaustion ⇒ `BeamError::SystemLimit`; heap exhaustion during delivery ⇒ propagate the
  underlying `BeamError::Alloc` after a collect-and-retry. `beam_term::TermError` is reconciled
  upstream, so `?` crosses the boundary.
- **`unsafe` is confined** to the modules that touch raw words: `signal.rs`/`monitor.rs` message
  builders (writing tuple/ref words through `beam_alloc::Heap::alloc_words`) if needed. Every
  `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`../beam_common/src/safety.rs` convention). `unsafe_op_in_unsafe_fn` stays clean. `pid.rs`,
  `registers.rs`, `run_queue.rs`, `link.rs`, `monitor.rs` set logic, `ptab.rs` bookkeeping and
  `progress.rs` must be safe Rust.
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
  `src/ptab.rs` (the table + signal hub) is the only file over that figure, and **only**
  because of its comprehensive inline `#[cfg(test)]` module (≈830 lines of production code + ≈460
  lines of tests) — it has one responsibility and deliberately stays whole; do not split it for size
  alone.
## Test Strategy
- In-module `#[cfg(test)] mod tests` per module: `pid` (encode/decode round-trip vs `beam_term`
  accessors, boundary fields, `from_eterm` rejects a non-pid), `registers` (live bounds, get/set range
  errors, stack push/pop, `gather_roots`/`scatter_roots` round-trip incl. the split point),
  `run_queue` (FIFO order, `contains`/`remove`), `link` (`add` idempotence, `remove`, `drain`),
  `monitor` (`RefAllocator` monotonic, `Monitors` push/remove/find, `MonitorRef::to_eterm` shape),
  `signal` (`exit_policy` table: normal/kill/other × trapping/not; `build_exit_message`/
  `build_down_message` arity + head atom), `progress` (generation bump), `ptab` (alloc/remove/reuse +
  serial bump, `register`/`whereis`/`unregister`, stale-pid rejection, `deliver`/`deliver_simple`
  `Enqueued`-vs-`Woke` + the wake list, `take` clearing a pending wake, and the trap/link-`EXIT`/
  `{'DOWN'}` signal paths waking a blocked recipient).
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
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
  beam_process` and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml
  --workspace`. Always go through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never
  touch `../../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness,
  not here.
## Design Decisions
- **Single scheduler, synchronous signal delivery (M1).** One `Scheduler` runs one process at a time;
  exit signals, links and monitors propagate **synchronously** through a worklist inside
  `ProcessTable::terminate`/`apply_exit_signal`, rather than through the C's decoupled per-process
  signal queue (`ErtsSignalPrivQueues`) serviced at schedule-in. This is simpler and equally correct
  for the common cases; the decoupled queue (needed for multi-scheduler fairness, `process_info` of
  pending signals, and signal ordering vs. messages) is a documented follow-up.
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
- **An exit reason delivered to a recipient is COPIED into that recipient's heap.** An exit/link/
  monitor reason that lives in the source process's heap (e.g. a tuple like `{badmatch, X}`) must not
  be embedded by reference into a recipient: the source heap is dropped when it terminates, which
  would leave a dangling pointer in the recipient's mailbox (and a safepoint GC of the recipient
  would then corrupt/crash). `enqueue_exit_message`/`enqueue_down_message` therefore `copy_struct` the
  reason into the recipient's heap first (with the source `OffHeapTable` as the copy context, draining
  the referenced MSO handles onto the `Message`), the cascade copies a peer-bound reason into the
  peer's own heap before queueing its termination, and `apply_exit_signal` copies from the still-live
  `signal.from` (self-signals embed directly, since the reason is already in the recipient's heap).
  Immediates and literal-area terms are shared by `copy_struct`, so the common reasons pay nothing.
  This mirrors `Process::deliver`'s copy-then-enqueue pattern and is what makes `{'EXIT', …}`/
  `{'DOWN', …}` survive the recipient's next collection.
- **GC root set = registers + mailbox payloads, gathered into a flat `&mut [Eterm]`.** `beam_gc`'s
  collector takes a flat root slice (`x_reg_array[0..Live)` + stack, no frame maps). The mailbox holds
  terms *in this process's heap*, so it is a sub-rootset (`beam_gc` explicitly delegates on-heap
  message queues to `beam_process`). Since `beam_message::Message` has no payload setter, `Process::
  collect` **drains the queue** (`Queue::dequeue`), appends each payload to the root buffer, runs the
  collector, scatters the registers back, and **rebuilds** each `Message` from the relocated payload
  (via the public `Message::new(..).with_token(..).with_off_heap(..)` builder). MSO handles are
  off-heap and do not move. This uses only `beam_message`'s public API — no sibling crate is modified.
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
- **`trap_exit` is a flag on the PCB, not a separate queue.** `exit_policy` reads it and decides
  `Terminate`/`Trap`/`Ignore`; the `Trap` case enqueues `{'EXIT', From, Reason}` into the mailbox. This
  is exactly the observable `process_flag(trap_exit, true)` behaviour.
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
- **The decoupled signal queue** (`ErtsSignalPrivQueues`): exit/link/monitor signals are propagated
  synchronously; the queue, its ordering guarantees vs. messages, and "pending signal" introspection
  are follow-ups.
- **Priority run queues** (max/high/normal/low), `erlang:process_flag(priority, …)` and port tasks.
- **Process dictionary** (`erl_process_dict.c`): the per-process `put/get/erase` term dictionary is
  not modelled; it is a sub-rootset the collector must also scan — a follow-up (as is
  `erlang:process_info/1`'s view of it).
- **Saved BIF/NIF arguments and heap fragments** as extra sub-rootsets (`erts_proc_sig_queue` fragments,
  `ErlHeapFragment` copy-in) — follow-ups; M1 assumes a message fits the receiver's young heap
  (collect-and-retry once).
- **Distribution-wide links/monitors** (`erl_monitor_link.c`'s node part): only local pids are linked/
  monitored; a link/monitor to an external pid is rejected or ignored, not routed through `beam_dist`.
- **`PortTable` is id allocation only** — port scheduling, port signals and port links/monitors are
  `beam_io`'s.
- **Exact exit-reason term from `beam_common::ExitReason`**: signal reasons are carried as live `Eterm`
  terms (the deliverable form), not the term-independent `beam_common::ExitReason`; the mapping between
  them belongs to the raising BIF layer (`beam_bifs`).
## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/pid.rs` → `ProcessId`/`PortId` (bit-exact internal pid/port immediates) + id limits
- `src/registers.rs` → `XRegs`, `Stack`, `Registers` (the root-set containers); `MAX_REG`/`MAX_ARG`
- `src/process.rs` → `Process` (PCB), `ProcessState`, `ProcessFlags`, `DeliverOutcome`, `Process::wake`, message delivery + GC entry
- `src/run_queue.rs` → `RunQueue` (the ready queue)
- `src/scheduler.rs` → `Scheduler`, `RunOutcome`, `ProcessRunner` (the engine seam), `Scheduler::resume_woken` (wake absorption)
- `src/progress.rs` → `ThreadProgress` (single-scheduler stub)
- `src/signal.rs` → `SignalAtoms`, `ExitSignal`, `ReasonClass`, `ExitPolicy`/`exit_policy`, the signal-message builders
- `src/link.rs` → `Links` (bidirectional link set)
- `src/monitor.rs` → `MonitorRef`, `MonitorKind`, `Monitor`, `MonitoredBy`, `Monitors`, `RefAllocator`
- `src/ptab.rs` → `ProcessTable` (pid alloc/lookup + name registry + signal/link/monitor hub + the wake list), `ExitEffect`, `PortTable`
- `src/error.rs` → `pub(crate)` error constructors (no new error type)
- `tests/scheduler.rs` → pid allocation + round-robin scheduling order + resume-on-send + counter ping-pong
- `tests/messaging.rs` → send into a mailbox + selective receive + `DeliverOutcome` wake reporting
- `tests/signals.rs` → link exit propagation (+ `trap_exit`) + monitor down-message
- `tests/gc.rs` → safepoint GC over the process root set
- `../../erts/emulator/beam/erl_process.{c,h}` → PCB, scheduler, run queues (read-only ground truth)
- `../../erts/emulator/beam/erl_ptab.{c,h}` → process/port table + name registry (read-only)
- `../../erts/emulator/beam/erl_monitor_link.c` → links/monitors (read-only)
- `../../erts/emulator/beam/erl_proc_sig_queue.c` → the signal queue being simplified away (read-only)
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
- The collector moves live terms, so **every** in-heap reference must be a root: registers, stack and
  (M1) the mailbox payloads. If you add an in-heap sub-rootset (process dictionary, saved args), add it
  to `Process::collect`'s gather/scatter in the same change.
## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types and the wave-3 dependency position.
- `../../CONTEXT.md` → workspace contracts (§1 A.1 process semantics, §3 error strategy, §5 GC/memory model, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A/§D → process/scheduler structure and the memory model.
- `../beam_gc/CONTEXT.md` (the collector + MSO this crate drives), `../beam_message/CONTEXT.md` (the
  mailbox `Queue` + `CopyContext`), `../beam_alloc/CONTEXT.md` (the per-process `Heap`), `../beam_term/CONTEXT.md` (the pid/port immediates).
