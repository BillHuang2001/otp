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
use beam_engine::{ExecutionContext, Machine, RunOutcome as EngineRunOutcome};
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
    /// The `budget` is accepted for the seam's sake but not enforced: the engine's
    /// blocking-aware entries run to the next `return`/`wait`, so Milestone 1 has no
    /// reduction pre-emption and a runner never reports
    /// [`Yield`](beam_process::RunOutcome::Yield) (see the crate `CONTEXT.md`).
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
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, _budget: u32) -> Result<RunOutcome> {
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

        let outcome = {
            let mut ctx = RuntimeContext::for_process(&mut *self.runtime, table, pid);
            // A BIF reached from this code must know its **call site**: the oracle derives
            // `erlang:load_nif/2`'s owning module from the call instruction, so a scheduled
            // process sets it exactly as `RuntimeContext::run_export` sets it for the
            // single-pass path (see [`RuntimeContext::enter_module`]).
            if let ProcessEntry::Export { module, .. } = &engine.entry {
                ctx.enter_module(module);
            }
            match engine.parked {
                // A resume continues the SAME machine: the registers, the Erlang stack and
                // the frames of the suspended `receive` must survive, so this deliberately
                // does not reset (see `Machine::resume_outcome`). Only a module-export
                // process can park, so a parked fun entry is unreachable — it is reported
                // as a typed capability gap rather than run as a wrong (or empty) function.
                Some((func, pc)) => match &engine.entry {
                    ProcessEntry::Export { module, .. } => {
                        engine.machine.resume_outcome(module, func, pc, &mut ctx)
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
                    } => engine.machine.run_export_outcome(
                        module,
                        *function,
                        *arity,
                        &engine.args,
                        &mut ctx,
                    ),
                    // A fun entry is applied synchronously: `apply_fun` routes an `-eval`
                    // closure back into the evaluator and a `.beam` fun into the engine's
                    // label entry — the evaluator is never re-implemented here. A returned
                    // value is the fun's result, so it records exactly like an export's.
                    ProcessEntry::Fun { fun } => ctx
                        .apply_fun(*fun, &engine.args)
                        .map(EngineRunOutcome::Returned),
                },
            }
        };

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

    #[test]
    fn a_returned_value_is_not_the_exit_reason() {
        // The runner never yields in Milestone 1, and a process that returns exits with
        // the `normal` atom — NOT with the value it produced (the value is recorded
        // separately by `RunnerState::result`).
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
