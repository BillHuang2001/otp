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
/// `match families::dispatch(..)` in `crate::interpreter`.
#[allow(dead_code)] // no family constructs a `Step` yet (every stub returns `Ok(None)`)
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
    /// Finish the run, yielding `x(0)`.
    Done,
}
