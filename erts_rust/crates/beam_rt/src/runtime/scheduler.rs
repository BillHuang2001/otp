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
use std::time::{Duration, Instant};

use beam_common::{BeamError, Result};
use beam_process::{ProcessId, ProcessState, ProcessTable, Scheduler, SignalAtoms};
use beam_term::Eterm;

use crate::error;
use crate::runtime::Runtime;
use crate::runner::RuntimeRunner;
use crate::timers::{TimerAction, TimerTarget};

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
    /// * is **blocked with nothing left that could wake it** — the run queue drained, no live
    ///   port and no pending timer deadline — reported with the very
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
    /// run queue, no live port and no pending timer deadline.
    ///
    /// This is the pump's quiescence test ([`Runtime::run_scheduled`]): when it holds, no
    /// process can run, no port can deliver and no timer can fire, so a blocked process
    /// would block forever and the honest answer is [`beam_engine::blocked`].
    ///
    /// A port table that cannot be borrowed (a nested borrow) is reported as **not**
    /// quiescent: the pump then keeps stepping rather than declaring a stall it cannot
    /// substantiate.
    fn is_quiescent(&mut self) -> bool {
        if self.run_queue_len() != 0 {
            return false;
        }
        match self.ports.try_borrow() {
            Ok(registry) if !registry.is_empty() => return false,
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
                Ok(executed) => ran += executed,
                Err(err) => break Err(err),
            }
            // A heap-fragment delivery made *inside* the slices just run woke its receiver;
            // `run_slice` absorbs the table's own wake list, and this is the fragment
            // counterpart, so the receiver this pass just woke still runs in this pass.
            let woke = self.absorb_fragment_wakes(&mut table, &mut scheduler);
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
                    table.deliver_simple(pid, from, message)?;
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
}

#[cfg(test)]
mod tests;
