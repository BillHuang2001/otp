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
//! runner records it per pid in [`RunnerState::result`], where
//! [`Runtime::process_result`](crate::runtime::Runtime::process_result) exposes it after
//! the pass. The value lives on the runtime's pointer-stable arena, which Milestone 1
//! never resets, so it stays valid for the runtime's lifetime.
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

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, RunOutcome as EngineRunOutcome};
use beam_loader::Module;
use beam_process::{ProcessId, ProcessRunner, ProcessState, ProcessTable, RunOutcome};
use beam_term::{Atom, Eterm};

use crate::context::RuntimeContext;
use crate::runtime::Runtime;

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
/// atom. Anything else (a term-carrying `Structured` reason, or runtime plumbing such as
/// `TermNotFound`) has no atom of its own, so its [`Display`](std::fmt::Display) text is
/// interned as the reason instead: the failure stays observable and is never silently
/// replaced by a plausible-looking value.
fn exit_reason(runtime: &mut Runtime, error: &BeamError) -> Eterm {
    if let Some(name) = error.as_atom() {
        return runtime.intern(name);
    }
    if let BeamError::Raise(raise) = error {
        if let Some(name) = raise.reason.as_atom() {
            return runtime.intern(name);
        }
    }
    runtime.intern(&error.to_string())
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

        let outcome = {
            let mut ctx = RuntimeContext::for_process(&mut *self.runtime, table, pid);
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
                // `x(0)`, not an exit reason: record it so the caller can observe it, and
                // terminate the process normally.
                self.state.results.insert(pid, value);
                Ok(RunOutcome::Exit(normal(self.runtime)))
            }
            Ok(EngineRunOutcome::Wait { func, pc }) => {
                // The runner owns the state transition (the scheduler only takes the
                // process off the ready queue): a blocked process is `Waiting` until a
                // delivery wakes it, which is exactly what makes a later send resume it.
                engine.parked = Some((func, pc));
                if let Some(process) = table.get_mut(pid) {
                    process.set_state(ProcessState::Waiting);
                }
                self.state.engines.insert(pid, engine);
                Ok(RunOutcome::Wait)
            }
            Err(error) => {
                let reason = exit_reason(self.runtime, &error);
                self.state.exits.insert(pid, reason);
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
}
