//! The module-context-aware dispatch driver.
//!
//! [`Machine::drive`] is the loop that actually runs instructions: it owns the
//! `(func, pc)` position of the run and the **module context** those indices
//! belong to, and folds every [`Step`] the interpreter or an instruction family
//! returns back into that position.
//!
//! # Why the driver, not the loops, owns the context
//!
//! A `call_ext*` the host resolved through
//! [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)
//! is executed **in this machine** (see [`crate::machine::context`]): the callee's
//! instructions live in another module, so the loop's `(module, labels)` pair is no
//! longer fixed for the run. The `call_ext*` arms report the transfer as
//! [`Step::Enter`], the frame model records a continuation's module context on the
//! frame itself ([`Continuation::Address`]), and this loop re-binds the active
//! module (and the machine's literal area) at those points only — an intra-module
//! [`Step::Goto`] still costs nothing.
//!
//! The loop keeps the entry module **borrowed** (`&Module`, context `0`) and every
//! entered module **owned** through its [`ModuleContext`] handle, so the module
//! identity of a suspended frame survives a
//! [`RunOutcome::Wait`](crate::RunOutcome::Wait) — which is exactly what
//! [`Machine::resume_outcome`] needs, because it is deliberately not a
//! [`reset`](Machine::reset).

use std::sync::Arc;

use beam_common::Result;
use beam_loader::Module;

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter::{build_label_map, LabelMap};
use crate::Step;

use super::{Continuation, Machine, ModuleContext, RunOutcome};

impl Machine {
    /// The dispatch loop over a pre-built label map, with the module context the
    /// machine currently carries.
    ///
    /// * `module` / `labels` describe the **entry** module — context `0`. The map may
    ///   be handed in (a run entry that already resolved its own entry label) or
    ///   built here, exactly once.
    /// * The starting context is [`Machine::current_context`]: `0` for a fresh run,
    ///   and the context a suspended process blocked in for a resume. A non-zero
    ///   context must still be registered in [`Machine::contexts`] (a resume does not
    ///   reset, so it is), and its module is used for `(func, pc)` — the passed
    ///   `module` is only context `0`'s module.
    ///
    /// Reports a `receive` that finds no message as [`RunOutcome::Wait`] (leaving the
    /// machine untouched at the reported address, and the context in
    /// [`Machine::current_context`] for the resume), and a `return`/halt as
    /// [`RunOutcome::Returned`].
    ///
    /// Also latches the A.8 tracing gate **once per run**: the per-instruction
    /// call/return arms read the plain `Machine::trace_on` `bool` instead of making a
    /// `trace_enabled()` vtable call per instruction, so an untraced run pays a
    /// single `bool` load and no event is ever built. See the `Machine::trace_on`
    /// field docs for the mid-run semantics.
    pub(crate) fn drive(
        &mut self,
        module: &Module,
        labels: Option<&LabelMap>,
        mut func: u32,
        mut pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.trace_on = ctx.trace_enabled();
        // The entry module's label index. A run entry that already resolved its
        // entry label hands its map in; otherwise it is built here (still exactly
        // one build per run).
        let built: LabelMap;
        let entry_labels: &LabelMap = match labels {
            Some(map) => map,
            None => {
                built = build_label_map(module);
                &built
            }
        };
        // Bind the starting context. `reset` left a fresh run in context `0`; a
        // resume arrives in the context the process blocked in.
        let mut entered: Option<Arc<ModuleContext>> = if self.current_context() == 0 {
            self.entry_literals = Some(module.literals.clone());
            self.set_literals(module.literals.clone());
            None
        } else {
            let handle = self
                .module_context(self.current_context())
                .ok_or_else(|| error::unsupported("run: unknown module context"))?;
            self.set_literals(handle.module.literals.clone());
            Some(handle)
        };
        loop {
            // The module/labels the next instruction is resolved against. Re-borrowed
            // per iteration: a context switch replaces `entered` below.
            let (active_module, active_labels): (&Module, &LabelMap) = match &entered {
                Some(handle) => (&handle.module, &handle.labels),
                None => (module, entry_labels),
            };
            match self.step_instruction(active_module, active_labels, func, pc, ctx)? {
                Step::Next => pc += 1,
                Step::Goto { func: f, pc: p } => {
                    func = f;
                    pc = p;
                }
                Step::Enter {
                    context,
                    func: f,
                    pc: p,
                } => {
                    if context != self.current_context() {
                        self.switch_context(context)?;
                        entered = match context {
                            0 => None,
                            id => Some(
                                self.module_context(id).ok_or_else(|| {
                                    error::unsupported("run: unknown module context")
                                })?,
                            ),
                        };
                    }
                    func = f;
                    pc = p;
                }
                Step::Wait { func: f, pc: p } => {
                    return Ok(RunOutcome::Wait { func: f, pc: p })
                }
                Step::Done => return self.finish().map(RunOutcome::Returned),
            }
        }
    }

    /// Follow the current frame's continuation as a control-flow [`Step`].
    ///
    /// A [`Continuation::Halt`] finishes the run ([`Step::Done`]); an address in the
    /// **current** module context is an intra-module [`Step::Goto`], and an address in
    /// another context is a [`Step::Enter`] the driver must switch for. The two are
    /// distinguished here (one integer compare) rather than at every call site.
    pub(crate) fn continuation_step(&self) -> Result<Step> {
        match self.continuation()? {
            Continuation::Halt => Ok(Step::Done),
            Continuation::Address {
                func,
                pc,
                context,
            } if context == self.current_context() => Ok(Step::Goto { func, pc }),
            Continuation::Address {
                func,
                pc,
                context,
            } => Ok(Step::Enter {
                context,
                func,
                pc,
            }),
        }
    }
}
