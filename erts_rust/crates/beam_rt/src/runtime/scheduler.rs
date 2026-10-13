// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The **scheduler pass** family: the runtime's run loop and its entry points.
//!
//! Every `run_*`/pump entry point on [`Runtime`] lives here, together with the pass body they
//! share. A pass moves the **process table** and the **runner state** out of the runtime
//! ([`take_processes`]/[`put_processes`]) so the [`RuntimeRunner`] can hold `&mut Runtime` while
//! the [`Scheduler`] holds `&mut ProcessTable` — two borrows that cannot coexist inside one
//! value (see [`crate::runner`]); both go back when the pass ends, whatever its outcome.
//!
//! On top of that body sit
//!
//! * the entry points: [`Runtime::run_one_slice`], [`Runtime::run_slices`],
//!   [`Runtime::run_scheduler`], the blocking [`Runtime::run_scheduled`] (the CLI's `run`
//!   action) and the bounded [`Runtime::pump_scheduler`] (the `-eval` `receive` pump);
//! * the wake absorption the pass drives — the heap-fragment wake list
//!   ([`Runtime::note_fragment_wake`]/[`absorb_fragment_wakes`]) and the fired-receive-deadline
//!   list ([`absorb_timer_wakes`]);
//! * the pass's **port/timer/distribution halves** — [`Runtime::deliver_port_messages`]
//!   (implemented next to the port subsystem in [`crate::port`]), the timer half
//!   ([`fire_due_timers`]/[`timer_wait`]) and [`pump_dist`].
//!
//! The move-out dance, the bounded [`RUN_DEADLINE`]/[`RUN_STEP_SLICES`] pump semantics and the
//! [`TimerWait`] sleep policy are all defined here. The [`TimerWait`]-driven waits are what keep
//! a pass from spinning: a pass sleeps exactly as long as the earliest pending deadline needs,
//! and only when it has nothing else to do.

use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_dist::driver::ConnectionDriver;
use beam_dist::Dispatched;
use beam_process::{PortTable, ProcessId, ProcessState, ProcessTable, Scheduler, SignalAtoms};
use beam_term::Eterm;

use crate::dist::{LocalDeliverySink, NONODE_NAME};
use crate::error;
use crate::runtime::Runtime;
use crate::runner::{RunnerState, RuntimeRunner};
use crate::timers::{TimerAction, TimerTarget};
use crate::RuntimeContext;

/// The longest [`Runtime::pump_scheduler`] (the `-eval` receive pump) may sleep waiting for
/// the next timer deadline.
///
/// The scheduler entry points that own the whole pass — [`Runtime::run_scheduler`] and
/// [`Runtime::run_slices`] — wait for the earliest deadline (see [`TimerWait`]); the `-eval`
/// pump is a *bounded* step called from inside a running expression, so it waits at most
/// this long per call and reports `0` (nothing happened) when no deadline became due. A
/// `receive` that really wants to wait for a timer therefore sees it within a pump slice —
/// the delays a `send_after`-driven probe uses are far below it — while the pump itself can
/// never stall the evaluator indefinitely.
const TIMER_PUMP_SLICE: Duration = Duration::from_millis(100);

/// How long [`Runtime::run_scheduled`] (the CLI's `run` action) pumps the scheduler for a
/// process that has not terminated.
///
/// `run` drives the **full** scheduler — the run queue, the port poller/drivers, the timer
/// heap and the distribution half — because a workload is free to block and be resumed by a
/// later pass (a driver/port echo round-trip, say). A workload that never terminates (an
/// endless `receive … after T` loop, an unsolvable `receive`) would otherwise hang the
/// executable, so the pump is bounded by this wall-clock deadline and reports the honest
/// diagnostic "the process did not terminate" when it expires — never a wrong value.
///
/// Sixty seconds is far longer than any fixture this runtime runs and far shorter than a
/// user would tolerate. The deadline is a **safety net**, not the normal path: a process
/// blocked with nothing left that could wake it stops immediately (see
/// [`Runtime::run_scheduled`]), so a bare `wait` keeps its fast, honest answer.
pub const RUN_DEADLINE: Duration = Duration::from_secs(60);

/// How many scheduler slices one [`Runtime::run_scheduled`] pump step may run before the
/// wall-clock deadline is checked again.
///
/// Control must never be handed to a single unbounded pass: a process that keeps re-arming a
/// `receive … after T` deadline would keep `run_scheduler` (`TimerWait::Deadline`) busy
/// forever, so the deadline would never be reached. Every step is therefore bounded by this
/// slice budget and the deadline is tested between steps.
const RUN_STEP_SLICES: u64 = 1_000;

/// How long one scheduler pass may sleep waiting for the next timer deadline.
///
/// This is the timer counterpart of the port half's poll clamp: the loop never *spins*, it
/// sleeps exactly as long as the earliest deadline requires — and only when it has nothing
/// else to do.
#[derive(Clone, Copy)]
enum TimerWait {
    /// Never sleep: a **bounded** pass ([`Runtime::run_slices`]/[`Runtime::run_one_slice`])
    /// only fires deadlines that have already passed, so a step stays a step.
    Never,
    /// Sleep until the earliest pending deadline, however far away — the unbounded
    /// [`Runtime::run_scheduler`]. Termination is guaranteed: every wait is preceded by the
    /// removal of the deadline it waited for, so the heap strictly shrinks.
    Deadline,
    /// Sleep at most this long, once per pass — the bounded `-eval` pump
    /// ([`Runtime::pump_scheduler`], see [`TIMER_PUMP_SLICE`]).
    AtMost(Duration),
}

impl TimerWait {
    /// Whether the pass may sleep one more time, having already slept `waits` times.
    const fn allows(&self, waits: u32) -> bool {
        match self {
            TimerWait::Never => false,
            TimerWait::Deadline => true,
            TimerWait::AtMost(_) => waits == 0,
        }
    }
}

impl Runtime {
    /// Runs a **bounded** scheduler pass from inside the running code — the `-eval`
    /// evaluator's `receive` pump.
    ///
    /// It performs exactly [`Runtime::run_pass`]'s move-out dance (the process table and
    /// the runner state leave the runtime so the runner can hold `&mut Runtime` while the
    /// scheduler holds `&mut ProcessTable`), with two differences that matter to a caller
    /// that is *not* the entry point:
    ///
    /// * the slice count is always bounded (`max_slices`), because the caller is the
    ///   evaluator's `receive` and must never block (or hang) forever;
    /// * the wake list is absorbed **first** ([`Scheduler::resume_woken`]), so a process a
    ///   send made *outside* a scheduler slice woke is enqueued and run — the
    ///   send→wake→resume path a `receive` waiting for a spawned process's reply needs.
    ///
    /// The runtime's scheduler and runner state persist across calls (they are moved out
    /// and put back), so a pump resumes exactly where the previous one left off.
    ///
    /// Returns the number of slices that ran **plus** the port messages a live port
    /// delivered into their owner's mailbox (see [`Runtime::scheduler_pass`]); `0` means
    /// neither happened. The caller — the evaluator's `receive` — treats a non-zero answer
    /// as "the mailbox may have changed, rescan it", so a port message is picked up exactly
    /// like a message another process sent.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when an enqueued pid has no registered engine;
    /// * the process table's errors while terminating a process or delivering a message
    ///   between processes;
    /// * the port poller's/driver's failures and a nested borrow of the port table (a typed
    ///   capability gap).
    pub(crate) fn pump_scheduler(&mut self, max_slices: u64) -> Result<u64> {
        let (ran, delivered) =
            self.scheduler_pass(Some(max_slices), true, TimerWait::AtMost(TIMER_PUMP_SLICE))?;
        Ok(ran + delivered as u64)
    }

    /// Runs **one** scheduler slice (one process, once), returning how many slices ran
    /// (`0` or `1`).
    ///
    /// This is the step hook for observing a run in progress: the scheduler and its run
    /// queue persist, so `run_one_slice` then [`Runtime::run_scheduler`] continues where
    /// the slice left off.
    ///
    /// # Errors
    ///
    /// As [`Runtime::run_scheduler`].
    pub fn run_one_slice(&mut self) -> Result<u64> {
        self.run_slices(1)
    }

    /// Runs at most `max_slices` scheduler slices, returning how many ran.
    ///
    /// # Errors
    ///
    /// As [`Runtime::run_scheduler`].
    pub fn run_slices(&mut self, max_slices: u64) -> Result<u64> {
        self.run_pass(Some(max_slices))
    }

    /// Runs the scheduler until the run queue is empty, returning the number of slices
    /// executed.
    ///
    /// A process blocked on an empty `receive` leaves the queue and simply stays
    /// `Waiting`; a message delivered to it **during** a slice is absorbed at the end of
    /// that slice (the scheduler's `resume_woken`), so a single `run_scheduler` call
    /// carries a send→wake→resume through to completion. A process that is waiting for a
    /// message nobody will send simply stays `Waiting` and the loop ends — it never hangs.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when an enqueued pid has no registered engine;
    /// * the process table's allocation/propagation errors while terminating a process
    ///   or delivering a message between processes.
    ///
    /// A raise *inside* a process is not an error here: it terminates that process with
    /// the raised reason (observable through [`Runtime::process_exit`]) and the pass
    /// continues.
    pub fn run_scheduler(&mut self) -> Result<u64> {
        self.run_pass(None)
    }

    /// **The CLI's `run` action.** Loads `path`, spawns `function/arity` as a real process
    /// and pumps the runtime's full scheduler until it terminates.
    ///
    /// This is [`Runtime::run_function`] made blocking-safe. `run_function` performs a
    /// **single** engine pass, so a workload that suspends — a `receive` waiting for a
    /// `{Port, {data, …}}` message from a driver, say — has nothing to resume it and reports
    /// [`beam_engine::blocked`]. Here the export runs as an ordinary scheduled process
    /// ([`Runtime::spawn_process`] reuses this very `.beam`, so no code-path search
    /// happens) and the loop advances the scheduler — the run queue, the port
    /// poller/drivers, the timer heap and the distribution half (see
    /// [`Runtime::scheduler_pass`]) — so a later pass delivers the port's reply, wakes the
    /// receiver and carries it to its `return`.
    ///
    /// The loop ends when the process
    ///
    /// * **returned** — its `x(0)` is returned (recorded by the runner and stable for the
    ///   runtime's lifetime);
    /// * **raised** — the **original** [`BeamError`] is returned, so the CLI classifies the
    ///   raise (its class, its `unsupported` marker) exactly as the single-pass path did
    ///   (see [`Runtime::take_process_error`]);
    /// * is **blocked with nothing left that could wake it** — the run queue drained,
    ///   nothing left to poll (no live port and no registered signal watch) and no pending
    ///   timer deadline — reported with the very
    ///   [`beam_engine::blocked`] diagnostic the single-pass path reports, so a bare `wait`
    ///   keeps its immediate, honest answer instead of spinning out the deadline; or
    /// * **outlived [`RUN_DEADLINE`]** — an honest "did not terminate" diagnostic.
    ///
    /// Every step is **bounded** ([`RUN_STEP_SLICES`] slices, and a wait of at most
    /// [`PORT_POLL_SLICE`](crate::port::PORT_POLL_SLICE)), so the pump never busy-spins at
    /// 100 % CPU and the wall-clock deadline is always reached.
    ///
    /// # Errors
    ///
    /// The code server's error for `path`, [`crate::error::undef`] when `function/arity` is
    /// not exported by it, whatever the running code raises, and the two honest diagnostics
    /// described above.
    pub fn run_scheduled(
        &mut self,
        path: impl AsRef<Path>,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.boot_if_needed()?;
        // Load first: the module is cached under its **own** name, which is the name
        // `spawn_process` resolves (a hit skips the code-path search entirely).
        let module = self.code_server.load_path(path)?;
        let name = module
            .atom_name(module.name)
            .ok_or_else(|| error::unknown_module("<unnamed .beam>"))?
            .to_string();
        // `spawn_process` derives the arity from the arguments it seeds into `x(0..arity)`;
        // the CLI grammar guarantees the two agree, so a mismatch here is a caller bug and
        // is reported rather than silently running a different function.
        if args.len() != arity as usize {
            return Err(error::message(format!(
                "run: {}:{function}/{arity} given {} argument(s)",
                name,
                args.len()
            )));
        }
        let pid = self.spawn_process(&name, function, args.to_vec())?;
        let deadline = Instant::now() + RUN_DEADLINE;
        loop {
            if let Some(value) = self.process_result(pid) {
                return Ok(value);
            }
            if let Some(error) = self.take_process_error(pid) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(error::message(format!(
                    "run: {}:{function}/{arity} did not terminate within {} ms",
                    name,
                    RUN_DEADLINE.as_millis()
                )));
            }
            let (ran, delivered) = self.scheduler_pass(
                Some(RUN_STEP_SLICES),
                false,
                TimerWait::AtMost(crate::port::PORT_POLL_SLICE),
            )?;
            // Nothing ran, nothing was delivered and nothing is left that could produce a
            // wake: stop with the same honest diagnostic the single-pass path reports rather
            // than spinning out the deadline.
            if ran == 0 && delivered == 0 && self.is_quiescent() {
                return Err(beam_engine::blocked());
            }
        }
    }

    /// The error a process spawned with [`Runtime::spawn_process`] raised, once it has
    /// **failed**.
    ///
    /// [`Runtime::process_exit`] exposes only the reason *term*; this is the original
    /// [`BeamError`], which additionally carries the raise class and the `unsupported`
    /// classification [`crate::cli`] classifies by (see [`crate::runner::RunnerState`]).
    /// Taking it consumes it, so the first caller gets it and later ones get `None`.
    pub(crate) fn take_process_error(&mut self, pid: ProcessId) -> Option<BeamError> {
        self.runner_state.take_error(pid)
    }

    /// Whether the scheduler has nothing left that could wake a blocked process: an empty
    /// run queue, **nothing left to poll** (no live port *and* no registered signal watch —
    /// [`PortRegistry::has_nothing_to_poll`](crate::port::PortRegistry::has_nothing_to_poll))
    /// and no pending timer deadline.
    ///
    /// This is the pump's quiescence test ([`Runtime::run_scheduled`]): when it holds, no
    /// process can run, no port can deliver and no timer can fire, so a blocked process
    /// would block forever and the honest answer is [`beam_engine::blocked`].
    ///
    /// The predicate is the port layer's own "is there anything for a poll round to wait
    /// on?" question, not "is a port open": `os:set_signal/2`'s self-pipe is pollable with
    /// **zero** ports, so a runtime that only watches a signal must not be declared stalled
    /// while a caught signal could still deliver `{notify, Signal}`.
    ///
    /// A port table that cannot be borrowed (a nested borrow) is reported as **not**
    /// quiescent: the pump then keeps stepping rather than declaring a stall it cannot
    /// substantiate.
    fn is_quiescent(&mut self) -> bool {
        if self.run_queue_len() != 0 {
            return false;
        }
        match self.ports.try_borrow() {
            Ok(registry) if !registry.has_nothing_to_poll() => return false,
            Err(_) => return false,
            Ok(_) => {}
        }
        self.timers.next_deadline().is_none()
    }

    /// Records that a **heap-fragment** delivery woke `pid` (see
    /// [`Runtime::fragment_wakes`](Runtime)).
    ///
    /// Called by the sender
    /// ([`RuntimeContext::send_fragmented_message`](crate::context::RuntimeContext)) after
    /// it has enqueued the payload and woken the receiver: this is the fragment path's
    /// substitute for `ProcessTable`'s wake list, which it bypasses. The pid is enqueued by
    /// the next [`Runtime::scheduler_pass`]; recording one for a pid that re-blocks or dies
    /// first is harmless (the sweep skips it).
    pub(crate) fn note_fragment_wake(&mut self, pid: ProcessId) {
        self.fragment_wakes.push(pid);
    }

    /// Turns the pids a heap-fragment delivery woke into run-queue entries, returning how
    /// many were enqueued.
    ///
    /// The fragment counterpart of `beam_process`'s own wake sweep
    /// (`Scheduler::resume_woken`), with the same rule: a pid is enqueued only when it is
    /// still a **live `Runnable`** process that is **not already queued** — it may have
    /// terminated or re-blocked since the wake — and it resumes at its own priority.
    fn absorb_fragment_wakes(
        &mut self,
        table: &mut ProcessTable,
        scheduler: &mut Scheduler,
    ) -> usize {
        let wakes = std::mem::take(&mut self.fragment_wakes);
        let mut resumed = 0;
        for pid in wakes {
            let Some(process) = table.get(pid) else {
                continue;
            };
            if process.state() != ProcessState::Runnable || process.flags().in_run_queue() {
                continue;
            }
            let priority = process.priority();
            if let Some(process) = table.get_mut(pid) {
                process.flags_mut().set_in_run_queue(true);
            }
            scheduler.enqueue_with_priority(pid, priority);
            resumed += 1;
        }
        resumed
    }

    /// Registers and enqueues every child an `erlang:spawn*` created during the slices just
    /// run, returning how many became run-queue entries — the **spawn** counterpart of
    /// [`Runtime::absorb_fragment_wakes`].
    ///
    /// A slice executes against the **pass's** process table (the runtime's own is a
    /// placeholder for the duration, see [`Runtime::take_processes`]) but can reach neither the
    /// pass's runner state nor its run queue, so the native spawn allocates the child's pid in
    /// that table and records what is left here ([`Runtime::note_pending_spawn`]). This is
    /// where the record becomes a schedulable process:
    ///
    /// 1. the child's engine entry lands in the **pass's** runner state, so the next round has
    ///    something to run it with;
    /// 2. the pid goes on the **pass's** run queue at its own priority, exactly as
    ///    [`Runtime::enqueue_process`] does for a child spawned outside a pass.
    ///
    /// A pid whose process is no longer in the table, or one already queued, is skipped: the
    /// child is never run twice and no engine is fabricated for a slot that is gone.
    fn absorb_pending_spawns(
        &mut self,
        table: &mut ProcessTable,
        state: &mut RunnerState,
        scheduler: &mut Scheduler,
    ) -> usize {
        let pending = std::mem::take(&mut self.pending_spawns);
        let mut enqueued = 0;
        for spawn in pending {
            // The child is looked up **first**: a pid whose slot is gone (it cannot have run,
            // but nothing here assumes it) gets no engine entry and no run-queue slot.
            let Some(process) = table.get_mut(spawn.pid) else {
                continue;
            };
            if process.flags().in_run_queue() {
                continue;
            }
            // Exactly `Runtime::enqueue_process`'s bookkeeping, against the pass's table: the
            // flag is what makes a second enqueue a no-op, and the state is what
            // `Scheduler::resume_woken` tests before it would enqueue the child again.
            process.flags_mut().set_in_run_queue(true);
            process.set_state(ProcessState::Runnable);
            let priority = process.priority();
            state.spawn(spawn.pid, spawn.entry, spawn.args);
            scheduler.enqueue_with_priority(spawn.pid, priority);
            enqueued += 1;
        }
        enqueued
    }

    /// Runs a scheduler pass: the shared dance every `run_*` entry performs.
    ///
    /// The **process table** and the **runner state** are moved out of the runtime, so the
    /// runner can hold `&mut Runtime` while the scheduler holds `&mut ProcessTable` — two
    /// borrows that cannot coexist inside one value (see [`crate::runner`]). Both go back
    /// when the pass ends, whatever its outcome.
    ///
    /// Returns the number of slices that ran; the port half (see
    /// [`Runtime::scheduler_pass`]) is not counted here.
    ///
    /// # Errors
    ///
    /// As [`Runtime::scheduler_pass`].
    fn run_pass(&mut self, max_slices: Option<u64>) -> Result<u64> {
        // A bounded pass is a *step*: it fires deadlines that have already passed but never
        // sleeps on one. The unbounded pass owns the runtime until it is quiescent, so it
        // waits for the earliest deadline (a process blocked on a `receive` that only a timer
        // can answer is resumed by it — exactly as a port wakes one).
        let wait = match max_slices {
            Some(_) => TimerWait::Never,
            None => TimerWait::Deadline,
        };
        self.scheduler_pass(max_slices, false, wait)
            .map(|(ran, _)| ran)
    }

    /// Runs a scheduler pass **and drives the port poller and the timer heap**: the shared
    /// body of every `run_*` entry and of [`Runtime::pump_scheduler`].
    ///
    /// The move-out dance is [`Runtime::run_pass`]'s (the runner needs `&mut Runtime` while
    /// the scheduler needs `&mut ProcessTable`). On top of it this adds the runtime's
    /// **port half** — the loop that lets a live port wake a blocked process:
    ///
    /// 1. run the scheduler until the run queue drains (bounded by `max_slices` when given);
    /// 2. with no port live, stop — a runtime that never opened a port pays nothing;
    /// 3. otherwise poll the runtime's **one persistent** port poller for at most
    ///    [`PORT_POLL_SLICE`](crate::port::PORT_POLL_SLICE), run the port tasks readiness
    ///    scheduled, and **deliver** every `{Port, {data, …}}`/`{Port, {exit_status, …}}`
    ///    message into its **owning process's mailbox**
    ///    ([`Runtime::deliver_port_messages`], which uses `beam_process`'s own deliver path);
    /// 4. a delivery is what wakes a receiver blocked in a compiled `receive`, so it
    ///    enqueues the woken process ([`Scheduler::resume_woken`]) and the loop runs the
    ///    scheduler again — until nothing is delivered and the queue is empty.
    ///
    /// and its **timer half** — [`Runtime::fire_due_timers`] delivers every
    /// `erlang:send_after/3` message whose deadline has passed into the target's mailbox
    /// (the same deliver+wake+`resume_woken` path a port message takes). When nothing ran
    /// and nothing was delivered, the pass consults `wait` ([`TimerWait`]) and sleeps until
    /// the earliest pending deadline instead of ending — so a receiver a *timer* has to wake
    /// is resumed on the very same pass. A pass with no pending deadline, or one that is not
    /// allowed to wait, ends exactly as before.
    ///
    /// `resume_first` absorbs the table's pending wake list **before** the first slice — the
    /// behaviour [`Runtime::pump_scheduler`] needs (a send made *outside* a scheduler slice
    /// must be enqueued), which [`Runtime::run_pass`] does not do (its `run_ready` absorbs
    /// wakes at each slice's end anyway).
    ///
    /// Returns `(slices that ran, port/timer messages delivered)`.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when an enqueued pid has no registered engine;
    /// * the process table's errors while terminating a process or delivering a message
    ///   between processes (including a timer's message that does not fit the receiver's
    ///   heap);
    /// * the port poller's/driver's failures; a nested borrow of the port table is a typed
    ///   capability gap rather than a panic.
    fn scheduler_pass(
        &mut self,
        max_slices: Option<u64>,
        resume_first: bool,
        wait: TimerWait,
    ) -> Result<(u64, usize)> {
        self.boot_if_needed()?;
        let mut table = self.take_processes();
        let mut scheduler = std::mem::replace(&mut self.scheduler, Scheduler::new());
        let mut state = std::mem::take(&mut self.runner_state);
        // A wake produced *outside* a scheduler slice before this pass started (a `-eval`
        // send, say) must be enqueued first when the caller asked for it.
        if resume_first {
            scheduler.resume_woken(&mut table);
        }
        // A heap-fragment delivery is absorbed at the same points as the table's own wake
        // list, so a wake it produced *outside* a slice is enqueued before the first one.
        self.absorb_fragment_wakes(&mut table, &mut scheduler);
        let mut ran: u64 = 0;
        let mut delivered: usize = 0;
        let mut waits: u32 = 0;
        let outcome: Result<()> = loop {
            // `run_ready` also stops once its own budget is spent; the loop's budget counts
            // every slice across the rounds, so a bounded pass never overshoots.
            let budget = max_slices.map(|max| max.saturating_sub(ran));
            if budget == Some(0) {
                break Ok(());
            }
            let slices = {
                let mut runner = RuntimeRunner::new(self, &mut state);
                scheduler.run_ready(&mut table, &mut runner, budget)
            };
            match slices {
                Ok(executed) => {
                    ran += executed;
                    // Each executed slice is one process the scheduler switched to: credit it to
                    // the node-global context-switch counter behind
                    // `erlang:statistics(context_switches)` (see
                    // `Runtime::note_context_switches`).
                    self.note_context_switches(executed);
                }
                Err(err) => break Err(err),
            }
            // A `garbage_collect` request (see `Runtime::request_gc`) whose process
            // terminated in the slices just run can never be served — its heap is gone — so
            // the request set is pruned to the pass's live processes here, where the table
            // holds exactly them.
            self.prune_gc_requests(&table);
            // The **async-completion half** of `erlang:garbage_collect/2`: an
            // `{async, ReqId}` request (`Runtime::request_gc_async`) owes its requester
            // `{garbage_collect, ReqId, Result}`, and this pass boundary is where the runtime
            // emits it — a delivery may collect the *receiver's* heap, which a BIF (inside the
            // executing machine, whose root set is in the engine's Rust locals) must not drive
            // (see `Runtime::deliver_gc_completions`). A message delivered here wakes its
            // receiver through the table's own wake list, so the pass goes round again to run
            // it — the same resume-on-send path a port or timer message takes.
            match self.deliver_gc_completions(&mut table) {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => {
                    delivered += count;
                    scheduler.resume_woken(&mut table);
                    continue;
                }
            }
            // The **native-service half**: drain the `file_server_2` service's and its
            // io-devices' mailboxes and deliver each reply into the requesting process's
            // mailbox (contract A.9, `src/native/file_server.rs`). A reply is what lets a
            // caller blocked in `gen_server:call`'s `receive` make progress — the boot's
            // `{apply,{c,erlangrc,[]}}` directive — so it is counted into `delivered`; the
            // requesting process is normally the runtime's own `Runnable` `self_pid`, which
            // the delivery therefore does **not** enqueue, and which resumes through the
            // engine's blocking-aware entry rather than a scheduler slice.
            match self.service_file_requests(&mut table) {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => {
                    delivered += count;
                    scheduler.resume_woken(&mut table);
                    continue;
                }
            }
            // The port-monitor half of the exit obligation: a process that died in the slices
            // just run may have been watching a live port, and `beam_process`'s finalizer cannot
            // remove the matching *incoming* entry because this runtime threads no port table
            // into the termination. The dead watchers' recorded `(port, reference)` pairs are
            // swept here, where the port registry is free (see `crate::runtime::ets_owner`).
            // Nothing is delivered, so the pass does **not** go round again for it.
            if let Err(err) = self.drain_port_monitor_exits() {
                break Err(err);
            }
            // The ETS owner-death half: a process that died in the slices just run may have
            // owned ETS tables. The obligation its exit observer recorded is discharged here,
            // where the ETS registry is free (see `crate::runtime::ets_owner`); a transfer
            // message enqueues its (blocked) heir, so go round again to run it.
            match self.drain_ets_owner_exits(&mut table, &mut scheduler) {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => {
                    delivered += count;
                    continue;
                }
            }
            // A child an `erlang:spawn*` created *during* the slices just run is registered
            // and enqueued here, so the next round runs it (see `absorb_pending_spawns`).
            if self.absorb_pending_spawns(&mut table, &mut state, &mut scheduler) > 0 {
                continue;
            }
            // A heap-fragment delivery made *inside* the slices just run woke its receiver;
            // `run_slice` absorbs the table's own wake list, and this is the fragment
            // counterpart, so the receiver this pass just woke still runs in this pass.
            let woke = self.absorb_fragment_wakes(&mut table, &mut scheduler);
            // The **driver-monitor marking** half of the A.4 monitor family: a C driver that called
            // `driver_monitor_process` watches an Erlang process, and this runtime is the only party
            // that holds the live process table, so the surveillance is discharged here — every
            // monitored pid the table no longer holds (a death in the slices just run, or a monitor
            // installed on an already-dead pid) is marked, and the **same** round's port tasks below
            // then fire the driver's `ErlDrvEntry::process_exit` for it. Nothing is delivered, so the
            // pass does not go round again for it; a still-live port-registry borrow defers the scan
            // (marking is idempotent state in the port metadata, so the next pass retries naturally).
            if let Err(err) = self.drain_driver_monitor_exits(&table) {
                break Err(err);
            }
            // The port half: with a live port, read what it produced and deliver it into
            // the owning process's mailbox (which may wake a blocked receiver).
            match self.deliver_port_messages(&mut table, crate::port::PORT_POLL_SLICE) {
                Err(err) => break Err(err),
                // A fragment wake was absorbed above, so go round again and run it.
                Ok(0) => {}
                Ok(count) => {
                    delivered += count;
                    // The delivery may have woken a blocked receiver: absorb the wake list so
                    // the next round runs it.
                    scheduler.resume_woken(&mut table);
                    continue;
                }
            }
            // The distribution half: accept a pending inbound connection and drive every
            // live link's handshake one step (contract A.5, Phase 1). Non-blocking, so a
            // runtime with no node (or a node with no traffic) pays nothing; a link that
            // made progress counts as work, so a bare `-eval`/scheduler loop keeps
            // pumping while a peer is mid-handshake.
            match self.pump_dist() {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => delivered += count,
            }
            // The inbound-delivery half: route what the established links decoded into the
            // addressed local process's mailbox. A delivery is what wakes a receiver
            // blocked in a compiled `receive`, so it enqueues the woken process and goes
            // round again — the same resume-on-send path a port or timer message takes.
            match self.dispatch_dist_inbound(&mut table) {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => {
                    delivered += count;
                    scheduler.resume_woken(&mut table);
                    continue;
                }
            }
            // The **connecting (client) half** of distribution (contract A.5, Phase 2): drive
            // every outbound link one non-blocking step and flush the controls the process
            // layer's [`DistSignalRouter`](beam_process::DistSignalRouter) queued. The same
            // shape as `pump_dist` above — a runtime that is not a node, or a node with no
            // outbound link, pays nothing and a pass never blocks here.
            match self.pump_dist_client() {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => delivered += count,
            }
            // The client's inbound half: route what the established **outbound** links decoded
            // into the addressed local process's mailbox, gated exactly like the server half.
            // A delivery is what wakes a receiver blocked in a compiled `receive`, so it goes
            // round again on the same resume-on-send path a port or timer message takes.
            match self.dispatch_dist_client_inbound(&mut table) {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => {
                    delivered += count;
                    scheduler.resume_woken(&mut table);
                    continue;
                }
            }
            // The timer half: deliver every `erlang:send_after/3` message whose deadline has
            // passed (which may wake a blocked receiver, exactly like a port message) and
            // resume every process whose finite `receive … after T` deadline fired.
            match self.fire_due_timers(&mut table) {
                Err(err) => break Err(err),
                Ok(0) => {}
                Ok(count) => {
                    delivered += count;
                    scheduler.resume_woken(&mut table);
                    self.absorb_timer_wakes(&mut table, &mut scheduler);
                    continue;
                }
            }
            // Nothing ran, nothing was delivered and nothing was woken. With a pending
            // deadline the pass may still have work: sleep until it and go round again to
            // fire it (a bounded pass/clamped pump refuses, so a step stays a step).
            if woke > 0 {
                continue;
            }
            match self.timer_wait(wait, waits) {
                Some(wait_for) => {
                    waits += 1;
                    if !wait_for.is_zero() {
                        std::thread::sleep(wait_for);
                    }
                    continue;
                }
                None => break Ok(()),
            }
        };
        self.runner_state = state;
        self.scheduler = scheduler;
        self.put_processes(table);
        outcome?;
        Ok((ran, delivered))
    }

    /// The **distribution half** of a scheduler pass (contract A.5, Phase 1).
    ///
    /// Drives the live distributed node ([`crate::dist::DistNode::pump`]): publishes it
    /// to `epmd` on the first pass, accepts whatever inbound connections are pending and
    /// advances every link's handshake. `Ok(0)` for a runtime with no node name (the
    /// common case) or a node with nothing to do, so a pass never waits here — the node's
    /// sockets are non-blocking and only a peer that has already sent bytes is read.
    ///
    /// # Errors
    ///
    /// Only the listener's own `accept` failures. A *link*'s failure is retired and
    /// recorded by the node itself ([`crate::dist::DistNode::last_link_error`]), never
    /// raised: a hostile or broken peer must not be able to fail a scheduler pass.
    pub(crate) fn pump_dist(&mut self) -> Result<usize> {
        match self.dist.as_mut() {
            Some(node) => node.pump(),
            None => Ok(0),
        }
    }

    /// Routes the inbound control messages `driver` has queued into this runtime's
    /// **local process mailboxes** — the runtime side of `beam_dist`'s
    /// [`InboundSink`] seam (contract A.5).
    ///
    /// `beam_dist` owns the protocol: it frames a link, decodes an inbound control
    /// message and classifies it into a [`Delivery`](beam_dist::Delivery). Routing is
    /// the runtime's job, and this is the explicit entry: every decoded delivery goes
    /// to a [`LocalDeliverySink`] built over **this** runtime's process table, so a
    /// `DOP_SEND*` reaches the addressed local process's mailbox through
    /// `beam_process`'s own delivery path (a blocked receiver is placed on the
    /// table's wake list, so the next pass resumes it) and a `DOP_REG_SEND*` is
    /// resolved against the runtime's own name registry.
    ///
    /// The decode runs on the **runtime's own atom table**, so the atom indices a
    /// delivery carries are exactly the ones `whereis` and the engine compare — a
    /// channel whose indices came from another table could address the wrong process,
    /// so the invariant is kept inside this method rather than left to the caller.
    /// `heap` is the scratch arena the decoded terms are materialised on; the
    /// delivery copies each term into the receiver's own heap, so nothing outlives
    /// it.
    ///
    /// Returns one [`Dispatched`] record per queued control message, in arrival
    /// order, carrying the classified delivery and what the sink did with it. The
    /// outcomes are honest: a delivery this runtime cannot route is
    /// [`DeliveryOutcome::Unsupported`], an addressed process that is not a process of
    /// *this* node is [`DeliveryOutcome::NoSuchProcess`] — never a mis-delivery.
    ///
    /// A runtime that is not a distributed node has the fixed `nonode@nohost`
    /// identity, exactly the one [`crate::context`]'s node identity hands
    /// `beam_codec`: an addressed pid on any other node is then a foreign process.
    ///
    /// # Why this entry stays on the *classify-only* path
    ///
    /// The two **production** inbound drives — the listening node
    /// ([`crate::dist::DistNode::dispatch_inbound`]) and the connecting half
    /// ([`DistClient::dispatch_inbound`](crate::dist_client::DistClient::dispatch_inbound))
    /// — both drain through a driver's `dispatch_inbound_checked`, so every delivery a
    /// real peer sends is validated against **its own** link identity. This entry is
    /// the *explicit, single-driver* seam (`runtime.rs`: "an explicit caller has
    /// already asked"), and it cannot perform that per-link check soundly: the caller
    /// supplies an arbitrary hand-built [`ConnectionDriver`] whose own
    /// handshake names need not be this runtime's node identity, so validating against
    /// them would judge a message against a *foreign* identity — the identity check is
    /// only meaningful where the driver and the node were built from the same name,
    /// which is exactly what the two production drives guarantee and this seam cannot.
    /// It therefore stays on [`ConnectionDriver::dispatch_inbound`] (the semantic
    /// classification view); a caller that drives real wire bytes should use the
    /// production drives.
    ///
    /// # Errors
    ///
    /// A link-level decode/classification failure (a truncated or malformed control
    /// message). The caller decides what to do with the link — the scheduler pass
    /// retires it and records the reason ([`crate::dist::DistNode::last_link_error`])
    /// rather than failing the pass.
    pub fn dispatch_inbound(
        &mut self,
        heap: &mut Heap,
        driver: &mut ConnectionDriver,
    ) -> Result<Vec<Dispatched>> {
        let (name, creation) = match self.dist.as_ref() {
            Some(node) => (node.node_name().to_string(), node.creation()),
            // Not a node: the fixed M1 local-node identity (creation 0), the same one
            // `crate::context::bridges`'s `NodeIdentity` gives `beam_codec`.
            None => (NONODE_NAME.to_string(), 0),
        };
        let atom = self.code_server.atoms_mut().intern(&name);
        let mut sink = LocalDeliverySink::new(&mut self.processes, atom, creation);
        driver.dispatch_inbound(heap, self.code_server.atoms_mut(), &mut sink)
    }

    /// The **inbound-delivery half** of a scheduler pass (contract A.5).
    ///
    /// Drains what every **established** link decoded and routes the `send` family
    /// into the addressed local process's mailbox ([`crate::dist::DistNode::dispatch_inbound`]
    /// over a [`LocalDeliverySink`] on the live process table), returning how many
    /// messages were really delivered.
    ///
    /// It is driven from inside [`Runtime::scheduler_pass`] — *not* from
    /// [`crate::dist::DistNode::pump`] — because the sink needs the pass's
    /// **`&mut ProcessTable`** at the same time as the runtime's atom table and the
    /// node, and the pass is the only place all three are live at once (the table is
    /// moved out of the runtime for the duration, see
    /// [`Runtime::take_processes`]).
    ///
    /// A no-op unless [`Runtime::allow_inbound_delivery`] turned the capability on
    /// (the default) **and** the node has an established link — so a runtime with no
    /// node, no traffic or the capability off pays one bool test per pass and never
    /// allocates the decode arena. All of that makes it `Ok(0)`, never a raise: a
    /// link-level failure retires just that link and records why
    /// ([`crate::dist::DistNode::last_link_error`]), so a hostile or broken peer can
    /// neither fail nor stall a scheduler pass.
    ///
    /// # Errors
    ///
    /// Never in practice (see above); the `Result` keeps the pass's call sites
    /// uniform.
    pub(crate) fn dispatch_dist_inbound(&mut self, table: &mut ProcessTable) -> Result<usize> {
        if !self.inbound_delivery {
            return Ok(0);
        }
        // Split borrows: the node and the code server are disjoint fields, so the
        // node's name/creation can be read while its own decode arena is used.
        let Some(node) = self.dist.as_mut() else {
            return Ok(0);
        };
        if !node.has_established_link() {
            return Ok(0);
        }
        let creation = node.creation();
        let name_atom = self.code_server.atoms_mut().intern(node.node_name());
        let mut sink = LocalDeliverySink::new(table, name_atom, creation);
        node.dispatch_inbound(self.code_server.atoms_mut(), &mut sink)
    }

    /// The **connecting half** of a scheduler pass (contract A.5, Phase 2).
    ///
    /// Drives the runtime's outbound distribution client
    /// ([`DistClient::pump`](crate::dist_client::DistClient::pump)): every link is stepped one
    /// bounded non-blocking step (its read is clamped to [`crate::dist::DIST_READ_SLICE`]), the
    /// driver's reply is written, and the controls the process layer's
    /// [`DistSignalRouter`](beam_process::DistSignalRouter) queued are encoded with the live
    /// atom table and flushed. A link that fails is **retired and recorded**
    /// ([`DistClient::last_error`](crate::dist_client::DistClient::last_error)), never raised —
    /// a hostile or broken peer must not be able to fail a scheduler pass.
    ///
    /// `Ok(0)` for a runtime that is not a node (the common case) or a node with no outbound
    /// link, so a pass never waits here.
    ///
    /// # Errors
    ///
    /// Only the shared cell's borrow conditions and the scratch heap's own allocation
    /// failures; a link's I/O failure is the recorded retirement described above.
    pub(crate) fn pump_dist_client(&mut self) -> Result<usize> {
        match self.dist_client.as_ref() {
            Some(client) => client.pump(self.code_server.atoms_mut()),
            None => Ok(0),
        }
    }

    /// The **inbound-delivery half of the client** (contract A.5, Phase 2).
    ///
    /// Drains what every **established outbound** link decoded and routes the `send` family
    /// into the addressed local process's mailbox, returning how many messages were really
    /// delivered. The mirror of [`Runtime::dispatch_dist_inbound`] (the listening node's half),
    /// with the same gates and the same sink, so both directions route through exactly one
    /// implementation.
    ///
    /// A no-op unless [`Runtime::allow_inbound_delivery`] turned the capability on (off by
    /// default, so the differential corpus stays byte-identical) **and** this runtime has a
    /// client — so a runtime with no node, no outbound link or the capability off pays one bool
    /// test per pass and never allocates the decode arena. Never raises in practice: a
    /// link-level failure retires just that link and records why.
    ///
    /// # Errors
    ///
    /// Never in practice (as above); the `Result` keeps the pass's call sites uniform.
    pub(crate) fn dispatch_dist_client_inbound(
        &mut self,
        table: &mut ProcessTable,
    ) -> Result<usize> {
        if !self.inbound_delivery {
            return Ok(0);
        }
        let Some(client) = self.dist_client.as_ref() else {
            return Ok(0);
        };
        // The node identity the sink resolves an addressed pid against: this runtime's own
        // `node@host` name and creation, exactly what the listening half uses. A client exists
        // only for a node, so the `None` arm is unreachable in practice — it keeps the fixed
        // non-node identity as the honest fallback rather than a panic.
        let (name, creation) = match self.dist.as_ref() {
            Some(node) => (node.node_name().to_string(), node.creation()),
            None => (NONODE_NAME.to_string(), 0),
        };
        let name_atom = self.code_server.atoms_mut().intern(&name);
        let mut sink = LocalDeliverySink::new(table, name_atom, creation);
        client.dispatch_inbound(self.code_server.atoms_mut(), &mut sink)
    }

    /// How long this pass may sleep waiting for the next timer deadline, or `None` when it
    /// must not (or cannot) wait — see [`TimerWait`].
    ///
    /// The wait is clamped to the earliest *live* deadline (cancelled tombstones are pruned
    /// on the way), so the pass never sleeps past the work it is waiting for.
    fn timer_wait(&mut self, policy: TimerWait, waits: u32) -> Option<Duration> {
        if !policy.allows(waits) {
            return None;
        }
        let deadline = self.timers.next_deadline()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        Some(match policy {
            TimerWait::Never => return None,
            TimerWait::Deadline => remaining,
            TimerWait::AtMost(max) => remaining.min(max),
        })
    }

    /// Fires every timer whose deadline has passed, delivering its message into the target's
    /// mailbox — or, for a **receive deadline**, recording the address its process must
    /// resume at — and returns how many deadlines took effect.
    ///
    /// A **pid** target is used as scheduled; a **name** target is resolved in `table` at
    /// fire time (as the oracle resolves a registered name). A target the table does not hold
    /// — a pid that died, a name nobody holds — receives nothing and is not an error (the
    /// oracle drops the message too); the timer is consumed either way, so it fires exactly
    /// once. The delivery is `beam_process`'s own `deliver_simple`, so a blocked receiver is
    /// woken on the table's wake list and the caller resumes it with
    /// [`Scheduler::resume_woken`] — the very path a port message takes.
    ///
    /// A [`TimerAction::Resume`] deadline (a finite `receive … after T`) instead records its
    /// `(func, pc)` in [`Runtime::timer_resumes`] and queues the pid in
    /// [`Runtime::timer_wakes`]; the caller makes the parked process runnable with
    /// [`Runtime::absorb_timer_wakes`], and the runner resumes it at the **timed-out** path
    /// with **no** message delivered.
    ///
    /// # Errors
    ///
    /// The process table's delivery errors (an allocation failure while copying the message
    /// onto the receiver's heap) — never a swallowed failure.
    fn fire_due_timers(&mut self, table: &mut ProcessTable) -> Result<usize> {
        let fired = self.timers.take_due(Instant::now());
        let mut effects = 0;
        for timer in fired {
            match timer.action {
                TimerAction::Deliver {
                    target,
                    from,
                    message,
                } => {
                    let target = match target {
                        TimerTarget::Pid(pid) => Some(pid),
                        TimerTarget::Name(name) => table.whereis(name),
                    };
                    let Some(pid) = target else {
                        continue; // a name nobody holds: the message is dropped
                    };
                    if !table.contains(pid) {
                        continue; // a pid that has already terminated
                    }
                    // The runtime's one **fragment-aware** delivery
                    // (`RuntimeContext::deliver_in_table`): a **relocation-exempt** receiver (a
                    // process under the `-eval` evaluator or a nested machine) refuses the
                    // collection an exhausted delivery would otherwise run, so the
                    // heap-fragment fallback is what lets the timer's message land.
                    RuntimeContext::deliver_in_table(self, table, pid, from, message)?;
                    effects += 1;
                }
                TimerAction::Resume { pid, func, pc } => {
                    if !table.contains(pid) {
                        continue; // a receiver that terminated before its deadline
                    }
                    self.timer_resumes.insert(pid, (func, pc));
                    self.timer_wakes.push(pid);
                    effects += 1;
                }
            }
        }
        Ok(effects)
    }

    /// Turns the pids a fired receive deadline woke into run-queue entries, returning how
    /// many were enqueued.
    ///
    /// The receive-timeout counterpart of [`Runtime::absorb_fragment_wakes`], with one extra
    /// step: a parked receiver is [`ProcessState::Waiting`], so it is made `Runnable` as it is
    /// enqueued. A pid that is no longer waiting — it terminated, or a message woke it and it
    /// is already queued — is **skipped**, and its recorded resume address is dropped so the
    /// message-woken resume cannot be hijacked by the stale timeout.
    fn absorb_timer_wakes(&mut self, table: &mut ProcessTable, scheduler: &mut Scheduler) -> usize {
        let wakes = std::mem::take(&mut self.timer_wakes);
        let mut resumed = 0;
        for pid in wakes {
            let parked = matches!(table.get(pid), Some(process)
                if process.state() == ProcessState::Waiting && !process.flags().in_run_queue());
            if !parked {
                self.timer_resumes.remove(&pid);
                continue;
            }
            let Some(process) = table.get_mut(pid) else {
                self.timer_resumes.remove(&pid);
                continue;
            };
            process.set_state(ProcessState::Runnable);
            process.flags_mut().set_in_run_queue(true);
            let priority = process.priority();
            scheduler.enqueue_with_priority(pid, priority);
            resumed += 1;
        }
        resumed
    }

    /// Moves the process table out of the runtime for a scheduler pass.
    ///
    /// The placeholder is an equivalent **empty** table sharing the runtime's atom table
    /// (so its signal atoms are the runtime's); it is only ever observed by a runtime-side
    /// accessor during the pass, never by the running code, which sees the real table.
    fn take_processes(&mut self) -> ProcessTable {
        let placeholder = ProcessTable::new(SignalAtoms::from_table(self.code_server.atoms_mut()));
        std::mem::replace(&mut self.processes, placeholder)
    }

    /// Puts back the process table taken by [`Runtime::take_processes`].
    fn put_processes(&mut self, table: ProcessTable) {
        self.processes = table;
    }

    /// Runs `f` with a **process table** and the runtime's live **port table** borrowed
    /// together — the short-borrow seam for a `beam_process` operation that takes both.
    ///
    /// Several `beam_process` seams take the two tables as *disjoint* parameters because the
    /// port table is a distinct type: `ProcessTable::port_closed(&mut self, ports, port,
    /// reason)`, `ProcessTable::{monitor_port, monitor_port_term, demonitor_port}` and the
    /// `*_with_ports` forms of [`Scheduler`] and [`ProcessTable`]. The port table, however,
    /// lives **inside** the runtime's shared `Rc<RefCell<PortRegistry>>` (`Runtime::ports`),
    /// reached through
    /// [`PortRegistry::port_table_mut`](crate::port::PortRegistry::port_table_mut), so a
    /// caller must borrow that cell to get at it. This is the one place that borrow dance
    /// lives for a caller that already holds the other table:
    ///
    /// * the `Rc` is cloned **first**, so the returned `RefMut` borrows the *clone* rather
    ///   than the runtime — which is what lets `table` be borrowed at the same time (the
    ///   `#![deny(unsafe_code)]` runtime may not alias);
    /// * the borrow is held for the **one `f` call**, and nothing else. A port BIF runs
    ///   *inside* a scheduler slice — `open_port`/`port_command`/`port_close` from
    ///   interpreted code — and borrows the same cell itself, so holding this borrow across
    ///   anything that runs Erlang (the `Scheduler`/`ProcessTable` `*_with_ports` forms,
    ///   say) turns every such BIF into the typed `unsupported` capability gap. That is why
    ///   the pass does **not** route its slices through `run_ready_with_ports`; it discharges
    ///   the same obligations at its boundaries instead (the dead watcher's outgoing
    ///   `PortWatch` pairs — see `crate::runtime::ets_owner`);
    /// * a **nested** borrow (a port operation re-entered from inside one) stays the typed
    ///   capability gap, never a panic.
    ///
    /// `f`'s own error is returned untouched.
    ///
    /// # Errors
    ///
    /// The typed `unsupported` capability gap when the port registry is already borrowed (a
    /// nested port operation), plus whatever `f` returns.
    ///
    /// [`RefMut`]: std::cell::RefMut
    pub fn with_port_table<R>(
        &self,
        table: &mut ProcessTable,
        f: impl FnOnce(&mut ProcessTable, &mut PortTable) -> Result<R>,
    ) -> Result<R> {
        let cell = Rc::clone(&self.ports);
        let mut registry = cell
            .try_borrow_mut()
            .map_err(|_| error::unsupported("the port table is already in use"))?;
        f(table, registry.port_table_mut())
    }
}

#[cfg(test)]
mod tests;

/// The two pins that live next to the code they pin: the **quiescence predicate** (a
/// portless runtime that only watches a signal is *not* stalled) and the **short-borrow
/// seam** ([`Runtime::with_port_table`]).
///
/// They are inlined here rather than added to the sibling [`tests`] module because both are
/// about this file's own borrow/quiescence pairing; the fixtures the sibling module needs
/// (committed `.beam`s, a booted runtime) are not.
#[cfg(test)]
mod quiescence_and_port_table_tests {
    use super::*;

    use beam_io::signal::{Disposition, Signal};

    use crate::port::watch;

    /// A `handle` disposition makes `os:set_signal/2`'s self-pipe worth polling — with
    /// **zero** ports open — so the pump must not declare quiescence while one is installed;
    /// and once the disposition is gone the very same runtime is quiescent again.
    ///
    /// A signal disposition is **process-global**, so the test serializes on the port
    /// layer's own lock ([`watch::test_guard`]) and restores `default` before it returns.
    #[test]
    fn a_pollable_signal_watch_keeps_a_portless_runtime_from_being_quiescent() {
        let _guard = watch::test_guard();
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        assert!(rt.is_quiescent(), "an idle runtime is quiescent");

        watch::install(Signal::SigUsr1, Disposition::Handle).expect("handle on sigusr1");
        assert!(
            !rt.ports
                .try_borrow()
                .expect("the port registry is free")
                .has_nothing_to_poll(),
            "a handled signal leaves something to poll with no port open"
        );
        assert!(
            !rt.is_quiescent(),
            "a pollable signal watch is work the pump still has, not a stall"
        );

        watch::install(Signal::SigUsr1, Disposition::Default).expect("restore the disposition");
        assert!(
            rt.is_quiescent(),
            "with the watch no longer pollable there is nothing left that could wake a process"
        );
    }

    /// The seam hands a caller the **process table** and the live **port table** together for
    /// one call, and a nested borrow of the shared port cell is the typed capability gap —
    /// never a panic.
    #[test]
    fn the_short_borrow_seam_hands_both_tables_out_and_a_nested_borrow_is_a_typed_gap() {
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        let self_pid = rt.self_pid();
        // The shape a pass has: the table is moved out of the runtime, so the caller owns it
        // while the runtime still provides the port half.
        let mut table = rt.take_processes();
        let mut other = ProcessTable::new(SignalAtoms::from_table(rt.code_server.atoms_mut()));
        let seen = rt
            .with_port_table(&mut table, |table, ports| {
                Ok((table.contains(self_pid), ports.len()))
            })
            .expect("the port registry is free");
        assert_eq!(
            seen,
            (true, 0),
            "the call sees the live process table and the (empty) live port table"
        );

        let nested = rt.with_port_table(&mut table, |_table, _ports| {
            rt.with_port_table(&mut other, |_table, _ports| Ok(()))
        });
        let error = nested.expect_err("a nested borrow of the port table is a typed gap");
        assert_eq!(
            crate::error::unsupported_reason(&error).as_deref(),
            Some("the port table is already in use")
        );
        rt.put_processes(table);
    }
}
