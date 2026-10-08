//! Execution-trace emission (`impl Machine`, contract A.8).
//!
//! The engine reports the `call`/`return_from`/`return_to` event family to its host
//! through [`ExecutionContext::trace_event`], gated by the single cheap
//! [`ExecutionContext::trace_enabled`] check the dispatch loop performs per
//! call/return instruction (`crate::interpreter::dispatch`). Everything in this
//! module runs **only** while that gate is on, so an untraced run never builds an
//! event.
//!
//! # Emission points (the whole scope)
//!
//! * `call`, `call_only`, `call_last` — the **local** callee, named by
//!   `Module::functions[func]` (both `call_ext*`-style external dispatch and the
//!   `call_fun`/`apply` family are out of scope; a fun application is not a
//!   `call` in the IR).
//! * `call_ext`, `call_ext_only`, `call_ext_last` — the **imported** callee, named
//!   by the per-run import table (`LabelMap::import_target`). Only the callee's
//!   *entry* is reported: the oracle emits no `return_to` for a BIF/cross-module
//!   completion (probed: `erlang:'+'/2` yields `call` + `return_from` and **no**
//!   `return_to`), so reporting one here would invent an event.
//! * `return` — the **returning** function (`return_from`) plus the function the
//!   frame returns **to** (`return_to`). A `return` whose continuation is
//!   `Continuation::Halt` (the bottom frame: the end of the run, or a return whose
//!   caller is outside the running module) reports nothing — the oracle's
//!   `return_to undefined`.
//!
//! The run's **entry** function is not reported: it is not a call instruction, and
//! the oracle's entry `call` event comes from a function-entry breakpoint on a
//! traced spawn rather than from a call site. A host that needs it knows the entry
//! MFA it asked for.
//!
//! # Infallibility
//!
//! Every helper here returns `()`. A name or function index the running module
//! cannot resolve, or an argument register outside the file, makes the helper
//! **skip** the event — it never raises and never mutates the machine, so a traced
//! run is byte-for-byte the run an untraced one would have produced.

use beam_loader::{ImportRef, Module};

use crate::context::{ExecutionContext, TraceEvent};
use crate::machine::Machine;

use super::LabelMap;

/// The `(module name, function name, arity)` of `Module::functions[func]`, or
/// `None` when the index is out of range or either name is not in the running
/// module's atom table.
fn function_mfa<'a>(module: &'a Module, func: u32) -> Option<(&'a str, &'a str, u32)> {
    let function = module.functions.get(func as usize)?;
    let name = module.atom_name(function.name)?;
    let module_name = module.atom_name(module.name)?;
    Some((module_name, name, function.arity))
}

impl Machine {
    /// Report a `call` event for a **local** call into `Module::functions[func]`.
    ///
    /// The MFA is the callee's *declared* `(name, arity)` and the arguments are the
    /// callee's `x(0..arity)`: for the compiled IR the `call` operand's arity and a
    /// function's declared arity are the same number, so the event's argument list
    /// is exactly the oracle's `{M,F,Args}`.
    pub(super) fn trace_call_local(
        &mut self,
        module: &Module,
        func: u32,
        ctx: &mut dyn ExecutionContext,
    ) {
        if let Some((module_name, function_name, arity)) = function_mfa(module, func) {
            self.trace_call(module_name, function_name, arity, ctx);
        }
    }

    /// Report a `call` event for an **imported** (`call_ext*`) call.
    ///
    /// The target is resolved through the same per-run import cache the call itself
    /// uses, so an unresolvable import is silently skipped (the call that follows
    /// reports its own typed error).
    pub(super) fn trace_call_import(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        target: ImportRef,
        ctx: &mut dyn ExecutionContext,
    ) {
        if let Ok((module_name, function_name, arity)) = labels.import_target(module, target) {
            self.trace_call(module_name, function_name, arity, ctx);
        }
    }

    /// Report the `return_from` (the returning function) and `return_to` (the
    /// function control returns to) pair for an `Instr::Return`.
    ///
    /// `returning` is the function index containing the `return` instruction and
    /// `caller` the function index its frame's continuation names (the frame is
    /// already the caller's: the compiler's `deallocate` — folded into
    /// `call_last`/`call_ext_last` — has popped the returning function's own frame
    /// before the `return` executes). The value is `x(0)`, what
    /// [`Machine::finish`](crate::Machine) would yield.
    pub(super) fn trace_return(
        &mut self,
        module: &Module,
        returning: u32,
        caller: u32,
        ctx: &mut dyn ExecutionContext,
    ) {
        // No `x(0)` (an empty register file) means there is nothing to report that
        // the run itself would observe as its return value.
        let Some(value) = self.x(0) else { return };
        if let Some((module_name, function_name, arity)) = function_mfa(module, returning) {
            ctx.trace_event(TraceEvent::ReturnFrom {
                module: module_name,
                function: function_name,
                arity,
                value,
            });
        }
        if let Some((module_name, function_name, arity)) = function_mfa(module, caller) {
            ctx.trace_event(TraceEvent::ReturnTo {
                module: module_name,
                function: function_name,
                arity,
            });
        }
    }

    /// Build the `call` event's argument slice from `x(0..arity)` and report it.
    ///
    /// The slice is marshalled through the machine's reusable argument scratch
    /// buffer — the same one the wide `gc_bif`/`call_ext` marshalling uses — cleared
    /// and given back empty, so a traced call allocates nothing once the buffer has
    /// grown to fit its widest arity.
    fn trace_call(
        &mut self,
        module_name: &str,
        function_name: &str,
        arity: u32,
        ctx: &mut dyn ExecutionContext,
    ) {
        let mut scratch = std::mem::take(&mut self.args_scratch);
        scratch.clear();
        scratch.reserve(arity as usize);
        let mut complete = true;
        for index in 0..arity {
            match self.x(index) {
                Some(value) => scratch.push(value),
                None => {
                    // An out-of-range register index can only come from a malformed
                    // module. Skip the event rather than raising: the trace hook
                    // must never change a run's outcome.
                    complete = false;
                    break;
                }
            }
        }
        if complete {
            ctx.trace_event(TraceEvent::Call {
                module: module_name,
                function: function_name,
                arity,
                args: &scratch,
            });
        }
        scratch.clear();
        self.args_scratch = scratch;
    }
}
