//! The **process runner**: the runtime's [`ProcessRunner`] behind the scheduler.
//!
//! `beam_process` owns the scheduler but must not know how a process executes — it
//! reaches the execution engine through the [`ProcessRunner`] seam
//! (`../beam_process/src/scheduler.rs`, and the orchestration contract in that crate's
//! `CONTEXT.md`). This module is the concrete implementation: it turns one reduction
//! slice into one engine entry (`run_export_outcome` for a process's first run,
//! `resume_outcome` for a process parked at a blocking `receive`) over a
//! [`RuntimeContext`] that runs **as that pid** in the caller-supplied process table.
//!
//! # The borrow strategy (why the table is moved out of the runtime)
//!
//! A scheduler pass needs, at the same time,
//!
//! * `&mut Runtime` — BIF dispatch, the shared atom table, the heaps (what
//!   [`RuntimeContext`] borrows), and
//! * `&mut ProcessTable` — the PCBs the scheduler runs, which
//!   [`Context::for_process`](crate::context::RuntimeContext::for_process) resolves each
//!   mailbox against.
//!
//! The second is a **field** of the first, so handing both out of one `Runtime` value is
//! impossible. [`Runtime::run_scheduler`](crate::runtime::Runtime::run_scheduler)
//! therefore *moves* the table (and this module's [`RunnerState`]) out of the runtime for
//! the duration of the pass and puts them back afterwards — the same shape as any
//! split-borrow, written out explicitly because the crate is `#![deny(unsafe_code)]` and
//! may not alias.
//!
//! # What the runner records (the observable result)
//!
//! A process that runs to its `return` exits with the reason `normal` — **not** with the
//! value it produced (that is the BEAM contract: `x(0)` is the *return value* of the
//! function, not the process's exit reason). The value would otherwise be lost, so the
//! runner copies it onto the runtime's pointer-stable arena
//! ([`Runtime::stabilize_term`](crate::Runtime::stabilize_term)) and records it per pid in
//! [`RunnerState::result`], where
//! [`Runtime::process_result`](crate::runtime::Runtime::process_result) exposes it after
//! the pass. The copy matters: a value taken out of the mailbox (a port's stream, say)
//! points into the *process's own* heap, which is dropped the moment it terminates. The
//! arena never moves its terms and Milestone 1 never resets it, so the recorded value
//! stays valid for the runtime's lifetime.
//!
//! A process that fails records the raise's reason term in [`RunnerState::exit`]
//! ([`Runtime::process_exit`](crate::runtime::Runtime::process_exit)) and exits with that
//! very term, so the failure is observable through the process table's normal exit
//! propagation too — never swallowed, never invented.
//!
//! Legacy C ground truth (read-only): `../../erts/emulator/beam/erl_process.c`
//! (`schedule`/`erts_schedule`), `beam_common.c` (`apply`/`return`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, RunOutcome as EngineRunOutcome, Tier};
use beam_loader::{Instr, Module, Operand};
use beam_process::{ProcessId, ProcessRunner, ProcessState, ProcessTable, RunOutcome};
use beam_term::{is_small, signed_value, Atom, Eterm};

use crate::context::RuntimeContext;
use crate::runtime::Runtime;
use crate::timers::TimerHandle;

/// The entry point of one runtime-scheduled process: what to run, with which arguments.
///
/// A process's entry point lives here (not on the `beam_process` PCB, which has no such
/// field) because only the runtime knows how to execute code: it is the runtime's own
/// pid → entry side table.
pub(crate) enum ProcessEntry {
    /// A module export: enter `module:function/arity` with the process's args.
    Export {
        /// The module the entry point lives in (shared with the code server's cache).
        module: Arc<Module>,
        /// The exported function's name atom, in that module's atom table.
        function: Atom,
        /// The arity the function is entered with (always `args.len()`).
        arity: u32,
    },
    /// A fun value: apply the fun to the process's args.
    ///
    /// This is what `erlang:spawn/1` (and the fun head of `spawn_link/1`/`spawn_opt/2`)
    /// creates: an `-eval` evaluator closure, or a `.beam` fun term. There is no engine
    /// [`Machine`] for it — the fun is applied through
    /// [`ExecutionContext::apply_fun`](beam_engine::ExecutionContext::apply_fun), which
    /// routes an `-eval` closure back into the evaluator (`crate::eval::apply_eval_closure`)
    /// instead of re-implementing the evaluator here.
    Fun {
        /// The fun term to apply, built on a heap that outlives the process (the
        /// runtime's arena or a pooled BIF heap — both are pointer-stable and never
        /// reset in Milestone 1).
        fun: Eterm,
    },
}

/// The engine state of one runtime-scheduled process.
struct ProcessEngine {
    /// The process's engine machine. It outlives every single slice: a `receive` that
    /// blocks resumes in **this** machine, so its registers, stack and frames survive
    /// (see `beam_engine`'s `RunOutcome`/`resume_outcome` protocol).
    machine: Machine,
    /// Where the process starts and what it is called with.
    entry: ProcessEntry,
    /// The first run's arguments (seeded into `x(0..arity)`); unused once the process
    /// has run, but kept so a re-run of a finished process is impossible to get wrong.
    args: Vec<Eterm>,
    /// The `(func, pc)` a `receive` suspended at, until the process is woken.
    ///
    /// Only an [`ProcessEntry::Export`] process can park: a fun entry is applied
    /// synchronously (the evaluator never returns a resume address), so `parked` stays
    /// `None` for it.
    parked: Option<(u32, u32)>,
    /// The pending **finite receive timeout** of a parked process, if its `wait_timeout`
    /// carried a statically known positive delay.
    ///
    /// While the process is parked the deadline lives in
    /// [`Runtime::timers`](crate::Runtime); this is the handle that cancels it when a real
    /// message wakes the receiver first (so the receive does not also fire its timeout). A
    /// `wait` with no timeout — including the `after infinity` the compiler emits as a plain
    /// `wait` — leaves it `None`, so the process parks indefinitely.
    parked_timeout: Option<TimerHandle>,
}

/// The per-pid engine state and recorded outcomes of every process the runtime spawned.
///
/// [`Runtime`] owns one; a scheduler pass *moves it out* so the
/// [`RuntimeRunner`] can hold it (and the runtime) at the same time as the scheduler
/// holds the process table — see the module docs.
#[derive(Default)]
pub(crate) struct RunnerState {
    /// The live engines, keyed by pid.
    engines: HashMap<ProcessId, ProcessEngine>,
    /// The `x(0)` of every process that ran to its `return`.
    results: HashMap<ProcessId, Eterm>,
    /// The exit reason term of every process that finished by raising.
    exits: HashMap<ProcessId, Eterm>,
    /// The **original** [`BeamError`] of every process that finished by raising.
    ///
    /// [`RunnerState::exits`] keeps only the reason *term* — what the process table
    /// propagates and [`Runtime::process_exit`](crate::runtime::Runtime::process_exit)
    /// exposes. A raise's **class** (`error`/`exit`/`throw`) and its `unsupported`
    /// classification live only on the error itself, so it is kept here too: the CLI's
    /// `run` action recovers it with
    /// [`Runtime::take_process_error`](crate::runtime::Runtime::take_process_error), which
    /// makes the pumped, scheduler-driven path classify exactly like the single-pass one.
    errors: HashMap<ProcessId, BeamError>,
}

impl RunnerState {
    /// Registers the engine of a freshly spawned process.
    pub(crate) fn spawn(&mut self, pid: ProcessId, entry: ProcessEntry, args: Vec<Eterm>) {
        self.engines.insert(
            pid,
            ProcessEngine {
                machine: Machine::new(),
                entry,
                args,
                parked: None,
                parked_timeout: None,
            },
        );
    }

    /// Registers a freshly spawned process whose entry point is a **fun value**.
    ///
    /// This is the `erlang:spawn/1` (and `spawn_link/1`/`spawn_opt/2`) path: `fun` is an
    /// `-eval` closure or a `.beam` fun term and it is applied to `args` when the process
    /// first runs (see [`ProcessEntry::Fun`]).
    pub(crate) fn spawn_fun(&mut self, pid: ProcessId, fun: Eterm, args: Vec<Eterm>) {
        self.spawn(pid, ProcessEntry::Fun { fun }, args);
    }

    /// Whether `pid` is a process this runtime spawned an engine for.
    pub(crate) fn contains(&self, pid: ProcessId) -> bool {
        self.engines.contains_key(&pid)
    }

    /// `pid`'s executing module and engine machine, when this runtime holds them.
    ///
    /// This is the read-only seam behind `erlang:process_info/2`'s **execution-position**
    /// items (`current_function`/`current_location`/`current_stacktrace`/`initial_call`,
    /// see [`crate::native::memory`]): a process's position lives in its
    /// [`Machine`] — the
    /// [`Machine::current_function`]-family accessors are the only builders of those
    /// terms — and only this table knows which machine belongs to which pid.
    ///
    /// `None` for:
    ///
    /// * a pid this runtime never spawned an engine for (the root `-eval` process, a
    ///   foreign pid);
    /// * a **fun-bodied** process ([`ProcessEntry::Fun`]): its entry is a fun value, so
    ///   there is no module to name the position's `M`/`F` atoms from — the position items
    ///   are the honest capability gap there, never a fabricated module;
    /// * the process whose machine the caller is *currently executing*: a scheduler pass
    ///   must take that one engine **out** of this map to drive it (see the module docs),
    ///   so it is simply absent while it runs.
    pub(crate) fn engine_at(&self, pid: ProcessId) -> Option<(&Module, &Machine)> {
        let engine = self.engines.get(&pid)?;
        match &engine.entry {
            ProcessEntry::Export { module, .. } => Some((module, &engine.machine)),
            ProcessEntry::Fun { .. } => None,
        }
    }

    /// The `x(0)` `pid` returned, once it ran to its `return`.
    pub(crate) fn result(&self, pid: ProcessId) -> Option<Eterm> {
        self.results.get(&pid).copied()
    }

    /// The raise reason `pid` finished with, once it failed.
    pub(crate) fn exit(&self, pid: ProcessId) -> Option<Eterm> {
        self.exits.get(&pid).copied()
    }

    /// Takes the **original** error `pid` failed with, once it failed.
    ///
    /// The reason *term* ([`RunnerState::exit`]) is what a process's exit propagates; the
    /// error itself is what carries the raise class and the `unsupported` classification,
    /// so the caller that must reproduce the CLI's diagnostics takes it here (see the field
    /// docs). Taking consumes it: the first caller gets it, later ones get `None`.
    pub(crate) fn take_error(&mut self, pid: ProcessId) -> Option<BeamError> {
        self.errors.remove(&pid)
    }
}

/// The `beam_process::ProcessRunner` the runtime hands to the scheduler.
///
/// It borrows the runtime (for BIF dispatch, atoms and heaps) and the
/// [`RunnerState`] moved out of it; the process table arrives per call, supplied by the
/// scheduler.
pub(crate) struct RuntimeRunner<'a> {
    /// The runtime the process's code runs on.
    runtime: &'a mut Runtime,
    /// The engines and outcomes (moved out of the runtime for the pass).
    state: &'a mut RunnerState,
}

impl<'a> RuntimeRunner<'a> {
    /// A runner over `runtime` and the moved-out `state`.
    pub(crate) fn new(runtime: &'a mut Runtime, state: &'a mut RunnerState) -> RuntimeRunner<'a> {
        RuntimeRunner { runtime, state }
    }
}

/// The reduction budget of **one scheduler slice**, charged as the machine runs.
///
/// The [`ProcessRunner`] contract puts the reduction count in the runner's hands (the
/// scheduler never inspects or resets `Process::reductions`), and `beam_engine` exposes no
/// mid-run reduction/step counter, so the count is derived from the one per-instruction
/// seam the executing machine reaches on its own: [`ExecutionContext::ensure_heap`], called
/// by `test_heap`/`allocate_heap` — the instructions the compiler emits once per loop
/// iteration that allocates (the ordinary Erlang shape).
///
/// One charge is one reduction-like unit. When the budget runs out, `charge` reports the
/// exhaustion through a typed error; the runner recognises it (the sticky [`exhausted`] flag
/// is set) and turns it into [`RunOutcome::Yield`] at the machine's recorded address rather
/// than a process failure.
///
/// An exhausted probe is **sticky**: it keeps refusing so a single exhaustion yields exactly
/// once (the first refusal propagates out of the run, so no second charge can occur — the
/// stickiness is what makes the runner's classification unambiguous).
///
/// [`exhausted`]: BudgetProbe::is_exhausted
struct BudgetProbe {
    /// The reduction units left in this slice. Never zero: a degenerate `budget` of 0 still
    /// gets one unit, so a slice always makes progress and the scheduler's run-queue loop
    /// cannot spin forever on an immediately-yielding process.
    remaining: u32,
    /// The reduction units charged so far (what the process's counter is set to).
    used: u32,
    /// Whether the budget ran out during this slice.
    exhausted: bool,
}

impl BudgetProbe {
    /// A probe with `budget` reduction units (at least one).
    fn new(budget: u32) -> BudgetProbe {
        BudgetProbe {
            remaining: budget.max(1),
            used: 0,
            exhausted: false,
        }
    }

    /// The reduction units this slice charged.
    fn used(&self) -> u32 {
        self.used
    }

    /// Whether the budget ran out during this slice.
    fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Charges one reduction unit, or reports the exhaustion.
    ///
    /// # Errors
    ///
    /// A typed `unsupported`-prefixed error once the budget is spent — the runner's signal
    /// that the slice must end in [`RunOutcome::Yield`]. It is **never** a process failure:
    /// the runner intercepts it and parks the machine instead of recording an exit.
    fn charge(&mut self) -> Result<()> {
        if self.remaining == 0 {
            self.exhausted = true;
            return Err(crate::error::unsupported(
                "preemption: the reduction budget for this scheduler slice is spent",
            ));
        }
        self.remaining -= 1;
        self.used += 1;
        Ok(())
    }
}

/// The **slice's** context: the [`RuntimeContext`] the executing machine runs on, with the
/// slice's reduction budget charged at its heap-admission points.
///
/// This wrapper exists because `beam_rt` may not change `beam_engine` or its own
/// `src/context/**`: it is handed to the machine the runner drives, and every context the
/// engine builds *inside* that run — an external module call
/// ([`RuntimeContext::run_export`]) or a fun application — is a plain `RuntimeContext`, so
/// the wrapper's [`ensure_heap`](ExecutionContext::ensure_heap) is reached by the slice's own
/// machine **only**. That is what makes the recorded resume address exact and keeps a nested
/// run from being mistaken for the pre-empted one.
///
/// Everything else is forwarded unchanged; the redirect is purely mechanical.
struct BudgetContext<'c, 'a> {
    /// The slice's real context.
    inner: &'c mut RuntimeContext<'a>,
    /// The slice's budget.
    probe: &'c mut BudgetProbe,
}

impl ExecutionContext for BudgetContext<'_, '_> {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        ExecutionContext::call_bif(self.inner, module, function, arity, args)
    }

    /// Charge one reduction, then serve the request by delegating (the runtime's arena is
    /// unbounded, so the delegation itself never fails on Milestone 1).
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.probe.charge()?;
        ExecutionContext::ensure_heap(self.inner, heap_need, live, x_regs, stack)
    }

    fn heap_available(&self) -> usize {
        ExecutionContext::heap_available(&*self.inner)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        ExecutionContext::alloc_words(self.inner, n)
    }

    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        ExecutionContext::apply(self.inner, module, function, arity, args)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        ExecutionContext::apply_fun(self.inner, fun, args)
    }

    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        ExecutionContext::send_message(self.inner, to, message)
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        ExecutionContext::receive_next(self.inner)
    }

    fn receive_skip(&mut self) -> Result<()> {
        ExecutionContext::receive_skip(self.inner)
    }

    fn receive_remove(&mut self) -> Result<()> {
        ExecutionContext::receive_remove(self.inner)
    }

    fn receive_reset(&mut self) -> Result<()> {
        ExecutionContext::receive_reset(self.inner)
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        ExecutionContext::receive_marker_reserve(self.inner)
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        ExecutionContext::receive_marker_bind(self.inner, marker, reference)
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        ExecutionContext::receive_marker_use(self.inner, reference)
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        ExecutionContext::receive_marker_clear(self.inner, reference)
    }
}

/// The `normal` exit-reason term (the reason a process that returns exits with).
fn normal(runtime: &mut Runtime) -> Eterm {
    runtime.intern("normal")
}

/// The exit-reason **term** for a `BeamError` raised while running a process.
///
/// An error that carries an atom reason — the standard `badarg`/`badarith`/… atoms, or a
/// raise whose reason is an atom (`unsupported_opcode: …`, `badmatch`, …) — becomes that
/// atom. A raise that carries a **structured** reason as a live term (`{badmap, M}`,
/// `{badkey, K}`, …) becomes that very term, so the process's exit reason is the real
/// value the producer built rather than a reshaped atom. Anything else (runtime plumbing
/// such as `TermNotFound`) has no atom of its own, so its
/// [`Display`](std::fmt::Display) text is interned as the reason instead: the failure
/// stays observable and is never silently replaced by a plausible-looking value.
fn exit_reason(runtime: &mut Runtime, error: &BeamError) -> Eterm {
    if let Some(name) = error.as_atom() {
        return runtime.intern(name);
    }
    if let BeamError::Raise(raise) = error {
        if let Some(name) = raise.reason.as_atom() {
            return runtime.intern(name);
        }
    }
    // A structured reason travels as a live term (the `beam_common::Raise` convention;
    // the payload is the reason as an `Eterm`, built on a BIF heap that M1 never
    // resets or moves). Return it directly — no reshaping, no loss.
    if let Some(term) = crate::error::live_reason_term(error) {
        return term;
    }
    runtime.intern(&error.to_string())
}

/// The finite timeout of the `receive` a process has just parked at `(func, pc)`.
///
/// The engine reports a suspension at the receive's **scan** address — the `loop_rec` label
/// the `wait`/`wait_timeout` jumps back to once a message arrives — and
/// [`EngineRunOutcome::Wait`] carries no timeout, so the *timed-out* continuation is
/// recovered from the module, whose compiled shape is fixed:
///
/// ```text
/// ScanLabel:  loop_rec Fail, Dst
///             … match tests, loop_rec_end ScanLabel …
/// Fail:       wait_timeout ScanLabel, Time
///             timeout            ; <-- the timed-out continuation
/// ```
///
/// `Fail` is exactly the jump target of the `loop_rec` at `pc`, and — per the C
/// (`emu/msg_instrs.tab:356-363`: "In case of timeout, control will be transferred to the
/// `timeout` instruction following the `wait_timeout` instruction", armed as
/// `*pi = $NEXT_INSTRUCTION` in `i_wait.int`) — a timeout resumes at the instruction
/// **right after** the `wait_timeout`, i.e. the `timeout` opcode.
///
/// Returns `None` when the process parked on a bare `wait` (the `after infinity` the compiler
/// emits as `wait`), and for a timeout whose delay is not statically readable (see
/// [`timeout_delay`]) — the process then parks indefinitely, exactly as before, rather than
/// being given an invented deadline.
fn parked_receive_timeout(
    machine: &Machine,
    module: &Module,
    func: u32,
    pc: u32,
) -> Option<(Duration, (u32, u32))> {
    let function = module.functions.get(func as usize)?;
    for (index, instr) in function.code.iter().enumerate() {
        let Instr::WaitTimeout { target, time } = instr else {
            continue;
        };
        // The `wait_timeout` that owns this receive is the one whose jump-back target is the
        // address the engine parked at (its `loop_rec`).
        if function.labels.get(target) != Some(&(pc as usize)) {
            continue;
        }
        let resume = (func, (index as u32) + 1);
        return timeout_delay(machine, module, time).map(|delay| (delay, resume));
    }
    None
}

/// The positive delay of a `wait_timeout`'s `Time` operand, when it is statically readable.
///
/// The operand is a **decoded IR operand**, not a runtime term: a literal timeout (`after
/// 20`) is [`Operand::Integer`] or (for a value the loader materialized) [`Operand::Literal`],
/// and an X-register timeout is read from the machine — the registers are untouched by the
/// suspension, so the value the `wait_timeout` tested is still there.
///
/// `None` for everything else — an `after infinity` atom, a **Y-register** timeout (a runtime
/// variable, which `Machine` exposes no stack read for), and any non-positive or non-integer
/// value. `None` means "no deadline": the process parks until a message arrives, which is the
/// honest answer when the delay cannot be known rather than a guessed one.
fn timeout_delay(machine: &Machine, module: &Module, time: &Operand) -> Option<Duration> {
    match time {
        Operand::Integer(millis) => positive_millis(*millis),
        Operand::Literal(index) => module.literal(*index).and_then(millis_of),
        Operand::X(register) => machine.x(*register).and_then(millis_of),
        _ => None,
    }
}

/// `millis` as a [`Duration`], or `None` when it is not a positive delay.
fn positive_millis(millis: i64) -> Option<Duration> {
    (millis > 0).then(|| Duration::from_millis(millis as u64))
}

/// The positive millisecond delay a timeout **term** denotes, or `None`.
///
/// A small integer greater than zero is a delay; the atom `infinity`, a zero/negative
/// integer, and anything else are not (`infinity` means "never", which is the absence of a
/// deadline — the same `None`).
fn millis_of(term: Eterm) -> Option<Duration> {
    if is_small(term) {
        return positive_millis(signed_value(term));
    }
    None
}

impl ProcessRunner for RuntimeRunner<'_> {
    /// Runs one slice of process `pid`: its first entry, or the resume of the `receive`
    /// that suspended it.
    ///
    /// # Reduction accounting and pre-emption
    ///
    /// `budget` is the slice's **reduction budget** (the C `CONTEXT_REDS`); the runner owns
    /// the reduction count (the scheduler never counts — see the [`ProcessRunner`]
    /// contract). It is charged by [`BudgetProbe`] at the machine's heap-admission points
    /// — every `test_heap`/`allocate_heap` instruction the executing machine runs reaches
    /// [`ExecutionContext::ensure_heap`] once — and the slice **ends in
    /// [`Yield`](beam_process::RunOutcome::Yield)** once the budget is spent, so a
    /// CPU-bound process is pre-empted and the scheduler requeues it instead of letting it
    /// monopolize the run queue.
    ///
    /// The yield is **resumption-exact**: the machine is parked at
    /// [`Machine::current_code`] — the very instruction whose `ensure_heap` ran out (the
    /// engine records it as the first statement of `step_instruction`, and re-running
    /// `ensure_heap` from outside is a no-op) — so the next slice resumes *at* that
    /// instruction with the registers, the stack and the frames intact, exactly as a
    /// `receive` resume does. No result is fabricated and no work is repeated.
    ///
    /// The budget is only charged from the **slice's own** machine: the wrapper is the
    /// context of that machine alone, and every machine a BIF reaches (an external module
    /// call, a fun apply) is built inside `RuntimeContext` with the plain context, so a
    /// nested run can never consume the budget nor be mistaken for the resumable machine.
    /// A loop the compiler emits **without** a heap test (`spin/1`-style: no allocation, so
    /// no `test_heap`) has no admission point and therefore runs to its next
    /// `return`/`wait` without being pre-empted — the honest limit, reported as such.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when `pid` has no registered engine (a pid the runtime did
    ///   not spawn cannot be run);
    /// * the process table's errors while a `send` delivered into a woken mailbox.
    ///
    /// A *raise inside the process* is **not** an error here: it becomes
    /// [`RunOutcome::Exit`] carrying the raised reason, exactly as the BEAM terminates a
    /// process with the reason it raised.
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, budget: u32) -> Result<RunOutcome> {
        // Take the engine out of the map: it is borrowed mutably for the whole slice while
        // `self.runtime` is borrowed by the context, and a `HashMap` cannot hand out a
        // `&mut` value while its own container is still reachable. It goes back in below
        // unless the process finished.
        let Some(mut engine) = self.state.engines.remove(&pid) else {
            return Err(crate::error::unknown_process_entry(pid));
        };

        // Settle a parked receive's deadline **before** the context borrows the runtime:
        // a deadline that fired resumes the process at the timed-out path, while a
        // message-wake cancels the deadline so the receive cannot also fire its timeout.
        if engine.parked.is_some() {
            match self.runtime.take_timer_resume(pid) {
                Some(address) => {
                    // The deadline won the race: resume at the `timeout` instruction with
                    // no message delivered (the mailbox is untouched, so the `after` clause
                    // runs).
                    engine.parked = Some(address);
                    engine.parked_timeout = None;
                }
                None => {
                    // A message woke the receiver first: drop the deadline. Cancelling one
                    // that already fired is a no-op, which is the honest read of the race.
                    if let Some(handle) = engine.parked_timeout.take() {
                        self.runtime.cancel_receive_timeout(handle);
                    }
                }
            }
        }

        // The **first** entry of a module-export process is the only run that goes through the
        // tier-aware entry below (a resume has no tier entry), so it is the one site that can
        // snapshot the module's baseline artifact for the `+JPperf` map. Gated on the flag
        // first: a disabled `+JPperf` (the default) must cost nothing at all.
        let first_entry_export = match &engine.entry {
            ProcessEntry::Export { module, .. }
                if engine.parked.is_none() && self.runtime.perf_map().config().map_enabled() =>
            {
                Some(Arc::clone(module))
            }
            _ => None,
        };

        let (outcome, used, exhausted) = {
            // The context also gets the **engines** of this pass (`self.state`): it drives
            // *this* process's machine (taken out of the map above), but a
            // `process_info/2` execution-position item must be able to read every *other*
            // scheduled process's position (see `crate::native::memory`).
            let mut ctx =
                RuntimeContext::for_process_with_engines(&mut *self.runtime, table, pid, self.state);
            // A BIF reached from this code must know its **call site**: the oracle derives
            // `erlang:load_nif/2`'s owning module from the call instruction, so a scheduled
            // process sets it exactly as `RuntimeContext::run_export` sets it for the
            // single-pass path (see [`RuntimeContext::enter_module`]).
            if let ProcessEntry::Export { module, .. } = &engine.entry {
                ctx.enter_module(module);
            }
            // The slice's reduction budget, charged at the machine's heap-admission points
            // (see the `run` docs).
            let mut probe = BudgetProbe::new(budget);
            let mut budget_ctx = BudgetContext {
                inner: &mut ctx,
                probe: &mut probe,
            };
            let outcome = match engine.parked {
                // A resume continues the SAME machine: the registers, the Erlang stack and
                // the frames of the suspended `receive` must survive, so this deliberately
                // does not reset (see `Machine::resume_outcome`). Only a module-export
                // process can park, so a parked fun entry is unreachable — it is reported
                // as a typed capability gap rather than run as a wrong (or empty) function.
                Some((func, pc)) => match &engine.entry {
                    ProcessEntry::Export { module, .. } => {
                        engine.machine.resume_outcome(module, func, pc, &mut budget_ctx)
                    }
                    ProcessEntry::Fun { .. } => Err(crate::error::unsupported(
                        "resume of a fun-bodied process (a fun entry never suspends)",
                    )),
                },
                None => match &engine.entry {
                    ProcessEntry::Export {
                        module,
                        function,
                        arity,
                    } => engine.machine.run_export_outcome_with_tier(
                        module,
                        *function,
                        *arity,
                        &engine.args,
                        &mut budget_ctx,
                        // Routed through the tier-aware entry so a scheduled process *can*
                        // execute on the native baseline tier: `Tier::BaselineJit` is the
                        // target, but the baseline **export** path does not record the
                        // machine's entry call (`beam_engine`'s `run_native_outcome` omits the
                        // `enter_run` its label sibling performs), so `process_info/2`'s
                        // `initial_call` cannot be answered for such a process. That is a
                        // `beam_engine` gap this crate may not patch, so the interpreter is
                        // selected until it lands.
                        Tier::Interpreter,
                    ),
                    // A fun entry is applied synchronously: `apply_fun` routes an `-eval`
                    // closure back into the evaluator and a `.beam` fun into the engine's
                    // label entry — the evaluator is never re-implemented here. A returned
                    // value is the fun's result, so it records exactly like an export's.
                    ProcessEntry::Fun { fun } => budget_ctx
                        .apply_fun(*fun, &engine.args)
                        .map(EngineRunOutcome::Returned),
                },
            };
            // The wrapper's borrow of the probe ends here (the machine is no longer
            // running), so the slice's accounting is readable.
            drop(budget_ctx);
            let used = probe.used();
            let exhausted = probe.is_exhausted();
            (outcome, used, exhausted)
        };

        // The `+JPperf` map (contract A.8) must describe the **real** native code this runtime
        // can run, never a fabricated range: the module's baseline artifact is that source, so
        // its cached `[start, size)` regions are snapshotted here -- once per first entry, not
        // per slice. Diagnostic only: `record_baseline_code` records, never propagates.
        if let Some(module) = first_entry_export.as_deref() {
            record_baseline_code(self.runtime, module);
        }

        // Pre-emption: the budget ran out at a heap-admission point of this slicing
        // machine. Park it at that very instruction and hand the process back to the
        // scheduler, which requeues it (`slice_end`'s `Yield` arm) so a second runnable
        // process gets the next slice. Nothing is recorded as a result/exit and the
        // process stays `Runnable` — it has neither returned nor blocked.
        if exhausted {
            let Some(address) = engine.machine.current_code() else {
                // Unreachable: only `ensure_heap` (reached from `test_heap`/
                // `allocate_heap`, which record their address first) charges the budget.
                // Reported as a typed error rather than parked at a guessed address — no
                // wrong result, no hang.
                return Err(crate::error::unsupported(
                    "preemption: the exhausted machine has no resumable instruction address",
                ));
            };
            if let Some(process) = table.get_mut(pid) {
                // The runner owns the reduction count: record what this slice spent so
                // `process_info(_, reductions)` reports real work.
                process.reset_reductions();
                process.bump_reductions(u64::from(used));
            }
            engine.parked = Some(address);
            // A parked receive's deadline was settled above; a yield has no deadline of its
            // own, so nothing is left armed here (a stale one would fire mid-run later).
            engine.parked_timeout = None;
            self.state.engines.insert(pid, engine);
            return Ok(RunOutcome::Yield);
        }

        match outcome {
            Ok(EngineRunOutcome::Returned(value)) => {
                // `x(0)`, not an exit reason: copy it onto the runtime's pointer-stable
                // arena (so it outlives the process, whose own heap dies with it) and
                // record it, then terminate the process normally.
                let value = self.runtime.stabilize_term(value)?;
                self.state.results.insert(pid, value);
                Ok(RunOutcome::Exit(normal(self.runtime)))
            }
            Ok(EngineRunOutcome::Wait { func, pc }) => {
                // The runner owns the state transition (the scheduler only takes the
                // process off the ready queue): a blocked process is `Waiting` until a
                // delivery wakes it, which is exactly what makes a later send resume it.
                engine.parked = Some((func, pc));
                // A `wait_timeout` with a statically known positive delay arms a deadline
                // now: when it fires the process resumes at the timed-out path instead of
                // waiting for a message (see `parked_receive_timeout`).
                engine.parked_timeout = None;
                if let ProcessEntry::Export { module, .. } = &engine.entry {
                    if let Some((delay, (resume_func, resume_pc))) =
                        parked_receive_timeout(&engine.machine, module, func, pc)
                    {
                        let handle = self.runtime.schedule_receive_timeout(
                            pid,
                            delay,
                            resume_func,
                            resume_pc,
                        );
                        engine.parked_timeout = Some(handle);
                    }
                }
                if let Some(process) = table.get_mut(pid) {
                    process.set_state(ProcessState::Waiting);
                }
                self.state.engines.insert(pid, engine);
                Ok(RunOutcome::Wait)
            }
            Err(error) => {
                let reason = exit_reason(self.runtime, &error);
                self.state.exits.insert(pid, reason);
                // Keep the original error as well: the reason *term* above is what the exit
                // propagates, but only the error carries the raise class and the
                // `unsupported` classification the CLI keys off (see `RunnerState::errors`).
                self.state.errors.insert(pid, error);
                Ok(RunOutcome::Exit(reason))
            }
        }
    }
}

/// Snapshots the baseline JIT's **real** code ranges of `module` into the `+JPperf` map
/// (contract A.8).
///
/// The perf map is the diagnostic that names the native code a scheduled process can run, so
/// it may only ever hold ranges the baseline backend actually mapped
/// ([`baseline_code_map`](beam_engine::jit::cache::baseline_code_map) -- which compiles the
/// module on first sight, so the ranges are its own artifact's) -- never a synthesised one.
/// Diagnostic-only by contract: a disabled flag returns without compiling anything, and a
/// compile or write failure is *recorded* in the map's own diagnostic slot rather than
/// propagated, exactly as the oracle logs it and carries on (a perf file must never fail a run).
fn record_baseline_code(runtime: &mut Runtime, module: &Module) {
    if !runtime.perf_map().config().map_enabled() {
        return;
    }
    match beam_engine::jit::cache::baseline_code_map(module) {
        Ok(regions) => {
            for region in regions {
                if let Err(error) =
                    runtime
                        .perf_map_mut()
                        .record(region.start, region.size, &region.name)
                {
                    runtime
                        .perf_map_mut()
                        .note_diagnostic(format!("perf: {error}"));
                }
            }
        }
        Err(error) => runtime
            .perf_map_mut()
            .note_diagnostic(format!("perf: no code map for this module: {error}")),
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the outcome bookkeeping: the recorded result, the mapped exit
    //! reason and the typed error for a pid with no engine. The end-to-end behaviour
    //! (blocking, resume-on-send) is `tests/scheduler.rs`.

    use beam_common::ExitReason;
    use beam_process::{ProcessTable, RunOutcome as ProcOutcome, SignalAtoms};
    use beam_term::{make_small, signed_value, AtomTable};

    use super::*;

    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    /// A process table independent of the runtime (what the scheduler owns).
    fn table() -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
    }

    #[test]
    fn a_pid_without_an_engine_is_a_typed_error() {
        let mut rt = runtime();
        let mut state = RunnerState::default();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");

        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        let error = runner
            .run(pid, &mut table, 1)
            .expect_err("no engine ⇒ a typed error");
        assert_eq!(
            error.to_string(),
            crate::error::unknown_process_entry(pid).to_string()
        );
    }

    #[test]
    fn the_standard_error_atoms_become_atom_reasons() {
        let mut rt = runtime();
        assert_eq!(
            exit_reason(&mut rt, &BeamError::BadArith),
            rt.intern("badarith")
        );
        assert_eq!(
            exit_reason(&mut rt, &BeamError::SystemLimit),
            rt.intern("system_limit")
        );
        // A raise whose reason is an OWNED atom (`BeamError::as_atom` cannot see it).
        let owned = BeamError::Raise(beam_common::Raise::error(ExitReason::atom(String::from(
            "unsupported_opcode: nope",
        ))));
        let reason = exit_reason(&mut rt, &owned);
        assert_eq!(rt.atom_name_of(reason), Some("unsupported_opcode: nope"));
    }

    #[test]
    fn a_reasonless_error_falls_back_to_its_diagnostic_text() {
        // `TermNotFound` carries no atom; the message is interned instead, so the
        // failure is still observable rather than silently dropped.
        let mut rt = runtime();
        let reason = exit_reason(&mut rt, &BeamError::TermNotFound);
        assert_eq!(rt.atom_name_of(reason), Some(BeamError::TermNotFound.to_string().as_str()));
    }

    #[test]
    fn the_recorded_result_is_readable_after_the_pass() {
        let mut state = RunnerState::default();
        let pid = ProcessId::new(7, 0);
        assert!(state.result(pid).is_none());
        assert!(state.exit(pid).is_none());
        state.results.insert(pid, make_small(41));
        assert_eq!(signed_value(state.result(pid).expect("recorded")), 41);
    }

    #[test]
    fn spawn_registers_an_engine_and_reports_it() {
        // A module is not needed to register the engine (the entry is only resolved when
        // the process runs), so this pins the bookkeeping alone.
        let mut state = RunnerState::default();
        let pid = ProcessId::new(3, 0);
        assert!(!state.contains(pid));
        let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
        let function = module.atom_table.lookup("add").expect("add is interned");
        state.spawn(
            pid,
            ProcessEntry::Export {
                module,
                function,
                arity: 2,
            },
            vec![make_small(2), make_small(3)],
        );
        assert!(state.contains(pid));
        assert!(state.result(pid).is_none());
    }

    #[test]
    fn spawn_fun_registers_a_fun_engine() {
        // The `erlang:spawn/1` bookkeeping: a fun-bodied process has an engine (so the
        // scheduler can run it and a re-enqueue is not a silent no-op), with no module
        // and no recorded outcome yet.
        let mut state = RunnerState::default();
        let pid = ProcessId::new(5, 0);
        assert!(!state.contains(pid));
        state.spawn_fun(pid, make_small(1), Vec::new());
        assert!(state.contains(pid));
        assert!(state.result(pid).is_none());
        assert!(state.exit(pid).is_none());
    }

    /// Whether `error` is the probe's typed pre-emption refusal -- the `unsupported`-prefixed
    /// signal the runner keys off to park the process instead of recording an exit.
    fn is_preemption(error: &BeamError) -> bool {
        crate::error::unsupported_reason(error)
            .is_some_and(|reason| reason.contains("reduction budget"))
    }

    /// A fresh probe is never exhausted; the `.max(1)` floor buys exactly one unit, so a slice
    /// can never spin; and the refusal past exhaustion is sticky (no unit is charged).
    #[test]
    fn a_zero_budget_still_charges_exactly_one_unit_and_then_refuses() {
        let mut probe = BudgetProbe::new(0);
        assert!(!probe.is_exhausted(), "a fresh probe is not exhausted");
        assert!(probe.charge().is_ok(), "the floor guarantees one charge");
        assert_eq!(probe.used(), 1);
        assert!(!probe.is_exhausted(), "one unit of progress is not exhaustion");

        let error = probe.charge().expect_err("the floor is spent");
        assert!(is_preemption(&error), "{error}");
        assert!(probe.is_exhausted());
        assert!(probe.charge().is_err(), "an exhausted probe stays exhausted");
        assert_eq!(probe.used(), 1, "no unit past the floor is charged");
    }

    /// A budget of `n` allows exactly `n` charges -- never exhausted while units remain -- and
    /// the `(n+1)`-th charge is the single refusal the runner turns into `RunOutcome::Yield`.
    #[test]
    fn a_budget_allows_exactly_its_units_then_refuses() {
        const N: u32 = 3;
        let mut probe = BudgetProbe::new(N);
        for charged in 1..=N {
            assert!(!probe.is_exhausted(), "units remain before charge {charged}");
            assert!(probe.charge().is_ok(), "charge {charged} of {N}");
            assert_eq!(probe.used(), charged);
        }
        assert!(!probe.is_exhausted(), "the budget is spent, not yet exceeded");

        let error = probe.charge().expect_err("the (n+1)-th charge is the refusal");
        assert!(is_preemption(&error), "{error}");
        assert!(probe.is_exhausted());
        assert_eq!(probe.used(), N, "the refusal charges nothing");
    }

    /// A scheduled export runs to completion through the runner's **tier-aware** first-entry
    /// path: its value is recorded and it exits `normal` (the selected tier is documented there).
    #[test]
    fn a_scheduled_export_runs_through_the_tier_aware_entry() {
        let mut rt = runtime();
        let mut state = RunnerState::default();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
        let function = module.atom_table.lookup("add").expect("add is interned");
        state.spawn(
            pid,
            ProcessEntry::Export {
                module,
                function,
                arity: 2,
            },
            vec![make_small(2), make_small(3)],
        );

        let outcome = {
            let mut runner = RuntimeRunner::new(&mut rt, &mut state);
            runner.run(pid, &mut table, 1_000).expect("the slice runs")
        };
        assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
        assert_eq!(
            signed_value(state.result(pid).expect("the value is recorded")),
            5,
            "the tier-aware entry computes the interpreted result"
        );
    }

    /// The `+JPperf` map is filled from the baseline artifact's **real** code ranges -- and a
    /// disabled flag (the default) compiles nothing and records nothing.
    #[test]
    fn the_baseline_code_map_fills_only_an_enabled_perf_map() {
        let module = beam_loader::load_file(slice_fixture()).expect("slice.beam");

        let mut off = runtime();
        assert!(!off.perf_map().config().map_enabled(), "+JPperf is off by default");
        record_baseline_code(&mut off, &module);
        assert_eq!(off.perf_map().entry_count(), 0, "a disabled map costs nothing");
        assert!(!off.perf_map().is_active(), "and creates no file");

        let dir = std::env::temp_dir().join(format!("beam_rt_runner_perf_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
        let mut perf = crate::perf_map::PerfMapConfig::new();
        perf.set_directory(&dir);
        perf.set_flag("map").expect("map is a grammar value");
        let mut on = Runtime::new(crate::config::RuntimeConfig::new().with_perf_map(perf))
            .expect("the runtime is created");

        record_baseline_code(&mut on, &module);
        assert_eq!(on.perf_map().error(), None);
        assert!(on.perf_map().entry_count() > 0, "the code map has real regions");
        let text = std::fs::read_to_string(on.perf_map().map_path().expect("the map is open"))
            .expect("the map is readable");
        for line in text.lines() {
            assert!(
                line.starts_with("0x") && line.contains(" $slice:"),
                "a real range of the module: {line:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }    #[test]
    fn a_returned_value_is_not_the_exit_reason() {
        // The runner DOES return `RunOutcome::Yield` once this slice's reduction budget is
        // spent (pre-emption is implemented -- see `BudgetProbe`), but that is not what this
        // pins: a process that *returns* exits with the `normal` atom -- NOT with the value it
        // produced (the value is recorded separately by `RunnerState::result`) -- so its
        // outcome is a termination, never a pre-emption.
        let mut rt = runtime();
        let reason = normal(&mut rt);
        assert_eq!(rt.atom_name_of(reason), Some("normal"));
        assert_ne!(
            ProcOutcome::Exit(reason),
            ProcOutcome::Yield,
            "a normal exit is a termination, not a pre-emption"
        );
    }

    /// The committed loader fixture, reused by relative path (never copied).
    fn slice_fixture() -> &'static str {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        )
    }

    // -- finite `receive … after T` deadline recovery -------------------------

    /// The committed finite-receive fixture, reused by relative path.
    fn recv_after_fixture() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/recv_after.beam")
    }

    /// `(function index, code index of the `wait_timeout`, scan code index)` for the finite
    /// receive of `module`'s `name/0`.
    ///
    /// The scan index is the code index of the `wait_timeout`'s jump-back label — the
    /// `loop_rec` the engine parks at (what it reports as the [`EngineRunOutcome::Wait`]
    /// address).
    fn finite_receive(module: &Module, name: &str) -> (u32, usize, u32) {
        let atom = module.atom_table.lookup(name).expect("the export is interned");
        let index = module
            .functions
            .iter()
            .position(|f| f.name == atom && f.arity == 0)
            .expect("the fixture defines it");
        let function = &module.functions[index];
        let (code_index, target) = function
            .code
            .iter()
            .enumerate()
            .find_map(|(i, instr)| match instr {
                Instr::WaitTimeout { target, .. } => Some((i, *target)),
                _ => None,
            })
            .expect("the export has a finite receive");
        let scan = function.labels.get(&target).copied().expect("the jump-back label");
        (index as u32, code_index, scan as u32)
    }

    #[test]
    fn a_finite_receive_recovers_its_timed_out_continuation() {
        // The compiled shape the runner reads, straight from `beam_disasm`:
        //   Scan: loop_rec Fail,Dst | … | Fail: wait_timeout Scan,{integer,T} | timeout | …
        // The engine parks at `Scan` (the `wait_timeout` jump-back target), and — per the C
        // (`emu/msg_instrs.tab:356-379`: on timeout control goes to the instruction
        // *following* `wait_timeout`) — the timed-out continuation is the `timeout` opcode.
        let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
        let machine = Machine::new();
        let (func, wait_index, scan) = finite_receive(&module, "after20");

        let (delay, resume) = parked_receive_timeout(&machine, &module, func, scan)
            .expect("after20 arms a 20 ms deadline");
        assert_eq!(delay, Duration::from_millis(20));
        assert_eq!(
            resume,
            (func, wait_index as u32 + 1),
            "resume at the instruction after wait_timeout"
        );
        assert!(
            matches!(module.functions[func as usize].code.get(wait_index + 1), Some(Instr::Timeout)),
            "the timed-out path is the `timeout` instruction"
        );
    }

    #[test]
    fn an_infinite_receive_arms_no_deadline() {
        // `after infinity` compiles to a bare `wait`: there is no `wait_timeout` to recover
        // a delay from, so the process parks indefinitely (never a fabricated deadline).
        let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
        let machine = Machine::new();
        let atom = module.atom_table.lookup("wait_forever").expect("wait_forever");
        let function = module.function(atom, 0).expect("wait_forever/0");
        assert!(
            !function.code.iter().any(|i| matches!(i, Instr::WaitTimeout { .. })),
            "a bare wait has no wait_timeout at all"
        );
        let scan = function
            .code
            .iter()
            .find_map(|instr| match instr {
                Instr::Wait { target } => Some(function.labels[target] as u32),
                _ => None,
            })
            .expect("the bare wait's resume label");
        let func = module.functions.iter().position(|f| f.name == atom).unwrap() as u32;
        assert_eq!(
            parked_receive_timeout(&machine, &module, func, scan),
            None,
            "an infinite receive never times out"
        );
    }

    #[test]
    fn a_timeout_delay_must_be_a_statically_known_positive_millisecond_count() {
        let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
        let machine = Machine::new();
        // A literal positive delay is a deadline.
        assert_eq!(
            timeout_delay(&machine, &module, &Operand::Integer(20)),
            Some(Duration::from_millis(20))
        );
        // Zero and negative are not delays (the engine handles a bad/`0` time itself), and
        // neither is a register the machine cannot resolve — an honest "no deadline" rather
        // than a guessed one.
        assert_eq!(timeout_delay(&machine, &module, &Operand::Integer(0)), None);
        assert_eq!(timeout_delay(&machine, &module, &Operand::Integer(-1)), None);
        assert_eq!(
            timeout_delay(&machine, &module, &Operand::X(0)),
            None,
            "an unset x register is no delay"
        );
        assert_eq!(
            timeout_delay(&machine, &module, &Operand::Y(0)),
            None,
            "a y (stack) register is not readable by the machine"
        );
    }
}
