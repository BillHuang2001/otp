//! The direct-threaded dispatch loop (`impl Machine`).
//!
//! One `match` arm per Milestone-1 [`beam_loader::Instr`] variant implements the
//! interpreter model of Part II §F without `unsafe`/computed-goto (the IR is
//! pre-decoded, so typed dispatch is equivalent to a jump table). The manager fills
//! in the instruction bodies; this skeleton exposes the frozen entry point.

use beam_common::Result;
use beam_loader::Module;
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::machine::Machine;

impl Machine {
    /// The direct-threaded dispatch loop over the Milestone-1 opcode subset.
    ///
    /// Entry point: function index `func` (into [`Module::functions`]) and code
    /// offset `pc`; runs to the function's `return` and yields `x(0)`.
    ///
    /// Skeleton: the manager implements the per-opcode bodies. Until then this
    /// returns the documented coverage-limit error and never panics.
    #[allow(dead_code)]
    pub(crate) fn run_loop(
        &mut self,
        module: &Module,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let _ = (module, func, pc, ctx);
        Err(crate::error::unsupported("interpreter"))
    }
}
