//! The per-instruction `Step` control-flow protocol.
//!
//! [`Step`] is the control-flow contract between the interpreter's dispatch loop
//! (`crate::interpreter`) and the per-instruction-family bodies
//! (`crate::families`). An instruction family performs its register/heap effect
//! and then tells the loop where to continue — it never mutates the loop's
//! `func`/`pc` directly.
//!
//! A raise is **not** a [`Step`]: it stays an `Err(beam_common::BeamError)` (the
//! exception layer either folds it into a `Step::Goto` to a catch handler or lets
//! it propagate out of the run).

/// The control-flow decision an instruction family returns to the dispatch loop.
///
/// Produced by the `crate::families` bodies; consumed by the interpreter's
/// `match families::dispatch(..)` in `crate::interpreter`, by the dispatch loop
/// (`Machine::run_loop_with_map`) and by the baseline JIT's per-instruction
/// helper (`crate::jit::baseline::jit_step_one`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Continue at `pc + 1` in the same function.
    Next,
    /// Jump within the module.
    Goto {
        /// The function index (into [`beam_loader::Module::functions`]).
        func: u32,
        /// The code offset within that function.
        pc: u32,
    },
    /// Suspend the process in a `receive`: the module resumes at this address when
    /// the wait is released by an arriving message.
    ///
    /// The dispatch loop reports it as [`crate::RunOutcome::Wait`] (leaving the
    /// registers/frames untouched so the host can re-enter at the same address via
    /// [`crate::Machine::resume_outcome`]); a **non-blocking** entry
    /// ([`crate::Machine::run_export`] and the JIT backends' `run_export`) folds it
    /// into the typed [`crate::error::blocked`] error instead.
    Wait {
        /// The function index (into [`beam_loader::Module::functions`]) to resume in.
        func: u32,
        /// The code offset within that function to resume at.
        pc: u32,
    },
    /// Switch the machine's **module context** and continue at `(func, pc)` in that
    /// module.
    ///
    /// Produced by the two places where control crosses a module boundary *inside*
    /// one machine: the `call_ext*` arms, when the host resolved the cross-module
    /// call through
    /// [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call),
    /// and a `return` (or an exception landing) whose target frame/continuation
    /// belongs to a module other than the one executing.
    ///
    /// `context` is a [`Machine`](crate::Machine)-owned module-context id: `0` is
    /// the module the run was entered in, `n > 0` indexes the seam-registered
    /// contexts. Unlike [`Step::Goto`] (a jump **within** the current context),
    /// this makes the driver rebind the module, its label map and the machine's
    /// literal area before executing `(func, pc)`.
    Enter {
        /// The module context to switch to (see [`Step::Enter`]).
        context: u32,
        /// The function index within that context's module.
        func: u32,
        /// The code offset within that function.
        pc: u32,
    },
    /// Finish the run, yielding `x(0)`.
    Done,
}
