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

//! Fun / closure instruction family.
//!
//! Owns the closure and fun-application instructions (per
//! `../beam_loader/CONTEXT.md`, "The instruction vocabulary"):
//! - [`Instr::MakeFun3`] — build a local closure from its `FunT` entry
//! - [`Instr::CallFun`] — call the fun in `x(Arity)`
//! - [`Instr::CallFun2`] — call the fun held in a register
//! - [`Instr::Apply`] — apply an `M,F` in `x(Arity)`/`x(Arity+1)` with `Arity` args
//! - [`Instr::ApplyLast`] — a tail `apply`
//!
//! Ground truth (read-only): `emu/instrs.tab` `i_make_fun3` (`:545`),
//! `i_call_fun`/`i_call_fun_last` (`:363`/`:371`), `apply`/`apply_last` (`:298`/
//! `:306`), and `beam_common.c` `call_fun` (`:1604`), `fixed_apply` (`:1528`),
//! `apply` (`:1433`).
//!
//! ## Semantics (the register conventions, verified against the C and the oracle)
//! - `MakeFun3 { index, dst, env }` — `index` is the **fun-table index** (the
//!   `FunT` entry *position*); `Module::fun(index)` yields `{function, arity,
//!   label, index, num_free, uniq}`, `env` holds `num_free` free-variable
//!   operands. The closure is built with
//!   [`terms::make_fun`](crate::terms::make_fun) and stored in `dst`. The header
//!   arity is `arity - num_free` — the *closure's* arity, i.e. the lambda's total
//!   arity minus the captured ones (`emu_load.c:645-650` passes
//!   `lambda->arity - lambda->num_free` to `erts_fun_entry_put`, and
//!   `emu/instrs.tab:565` asserts the header arity equals that).
//! - `CallFun { arity }` — the fun is in `x(arity)`; the arguments are
//!   `x(0)..x(arity-1)` (`beam_common.c:1615` `fun = reg[arity]`).
//! - `CallFun2 { tag, arity, func }` — the fun is the `func` operand (a register,
//!   normally `x(arity)`); the arguments are `x(0)..x(arity-1)`. `tag` is the
//!   compiler's type hint (`safe`/`unsafe`, or a fun-table index) and is not needed
//!   by the interpreter (`emu/generators.tab:26-49` rewrites `call_fun2` to
//!   `call_fun` with a `move`).
//! - `Apply { arity }` — `fixed_apply` (`beam_common.c:1536-1537`): the module is
//!   `x(arity)`, the function is `x(arity+1)`, and the arguments are
//!   `x(0)..x(arity-1)`. Both must be atoms. The target is resolved through the
//!   cross-module-call seam ([`ExecutionContext::resolve_external_call`]): on a hit
//!   the callee is entered **in this machine** with the caller's return address
//!   (`(func, pc + 1)`) stored in the live frame; otherwise it is dispatched
//!   through [`ExecutionContext::call_bif`](crate::context::ExecutionContext::call_bif).
//! - `ApplyLast { arity, deallocate }` — as `Apply`, but the frame is popped first
//!   (`i_apply_last`: `$deallocate($Deallocate)`). On the **synchronous** route
//!   control then follows the machine's continuation **context-aware** via
//!   [`Machine::continuation_step`](crate::machine::Machine::continuation_step):
//!   `Continuation::Halt` ⇒ `Step::Done`, an address in the **current** module
//!   context ⇒ `Step::Goto`, and an address in **another** context ⇒
//!   `Step::Enter { context, .. }` so the driver re-binds the module before
//!   executing the return address. This matters because the current function may
//!   have been entered through the cross-module
//!   [`resolve_external_call`](crate::ExecutionContext::resolve_external_call)
//!   seam: its tail `apply` must land back in the **caller's** module, never in the
//!   callee's. The shape is the same the core `CallExtLast` arm uses. On the
//!   **seam** route the callee is entered as `Step::Enter` (the popped frame is
//!   gone, so the callee's `return` follows the caller's continuation).
//!
//! Fun application (the `call_fun`/`call_fun2` path) reaches the host through the
//! in-machine **closure-apply** seam
//! [`ExecutionContext::resolve_local_fun`](crate::context::ExecutionContext::resolve_local_fun)
//! **first**: a host that owns the closure's defining module enters the lambda body
//! in this machine ([`Step::Enter`], with the free variables placed at
//! `x(arity..)`), so a `receive` inside the closure suspends the *calling* process;
//! on the seam's `Ok(None)` default the closure is applied through the
//! value-returning
//! [`ExecutionContext::apply_fun`](crate::context::ExecutionContext::apply_fun)
//! (default: a typed `unsupported`) as before. `apply`/`apply_last` name a
//! `module:function` directly and are resolved through the cross-module-call seam
//! [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)
//! — on a hit the callee runs in this machine ([`Step::Enter`]); on the
//! seam's `Ok(None)` default the target is applied through
//! [`ExecutionContext::call_bif`](crate::context::ExecutionContext::call_bif) as
//! before.
//!
//! Every failure is a typed [`beam_common::BeamError`]: `read_operand`/
//! `write_operand` propagate, an out-of-range register is
//! [`crate::error::bad_register`], a `make_fun3` with no matching descriptor is
//! [`crate::error::unsupported`], and a non-atom `apply` target is the C's
//! `badarg` **raise** — catchable through [`raise_badarg`] (landing in a live
//! `try`/`catch`), or the typed [`BeamError::BadArg`] when there is no catch
//! frame to land in (or the module cannot name `badarg`). A **host BIF error**
//! from an applied target is the same catchable raise — routed through
//! [`Machine::raise_bif_error`](crate::machine::Machine::raise_bif_error) — so it
//! lands in a live `try`/`catch` too rather than escaping the caller. Never a
//! panic and never a wrong result.

use crate::interpreter::LabelMap;

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{Instr, Module};
use beam_term::{atom_val, is_atom, make_atom, Eterm};

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::families::exceptions::callee;
use crate::machine::{Continuation, Machine};
use crate::operand::{
    operand_slack_words, read_operand, read_operands_slack, write_operand,
};
use crate::terms;
use crate::Step;

/// Execute a fun-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a fun opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    entry_module: &Module,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = labels;
    match instr {
        // `make_fun3 Index Dst Env` — build a closure from the `FunT` entry.
        Instr::MakeFun3 { index, dst, env } => {
            let descriptor = module
                .fun(*index)
                .ok_or_else(|| error::unsupported("make_fun3: fun-table index out of range"))?;
            // The `Env` operand list must carry exactly the lambda's free
            // variables; a mismatch is a malformed module, not a runtime value.
            if descriptor.num_free as usize != env.len() {
                return Err(error::unsupported(
                    "make_fun3: free-variable operand count mismatch",
                ));
            }
            // Reserve the closure block first (one rooted allocation), then read the
            // free-variable operands from the post-collection machine state. The
            // header arity is the *closure* arity: total lambda arity minus the
            // captured free variables (`emu_load.c`, `emu/instrs.tab:565`).
            let arity = descriptor
                .arity
                .checked_sub(descriptor.num_free)
                .ok_or_else(|| error::unsupported("make_fun3: num_free exceeds arity"))?;
            let extra: usize = env.iter().map(operand_slack_words).sum();
            let fun = terms::make_fun_rooted(
                machine,
                ctx,
                RootSet::ALL_LIVE,
                arity,
                descriptor.uniq as u64,
                descriptor.index,
                env.len(),
                extra,
                |m, slack| read_operands_slack(m, env, slack),
            )?;
            write_operand(machine, dst, fun)?;
            Ok(Some(Step::Next))
        }

        // `call_fun Arity` — the fun is in `x(arity)`, arguments in `x(0..arity)`.
        Instr::CallFun { arity } => {
            let fun = machine
                .x(*arity)
                .ok_or_else(|| error::bad_register(*arity))?;
            let args = register_args(machine, *arity)?;
            call_fun(machine, module, fun, &args, *arity, func, pc, ctx)
        }

        // `call_fun2 Tag Arity Func` — the fun is the `Func` operand, arguments in
        // `x(0..arity)`. The `Tag` hint (an atom or a fun-table index) is ignored.
        Instr::CallFun2 {
            arity,
            func: fun_operand,
            ..
        } => {
            let fun = read_operand(machine, ctx, fun_operand)?;
            let args = register_args(machine, *arity)?;
            call_fun(machine, module, fun, &args, *arity, func, pc, ctx)
        }

        // `apply Arity` — module = `x(arity)`, function = `x(arity+1)`, arguments
        // in `x(0..arity)` (`fixed_apply`).
        Instr::Apply { arity } => match apply_target(machine, entry_module, module, *arity, ctx)? {
            ApplyTarget::Called(result) => {
                machine.set_x(0, result)?;
                Ok(Some(Step::Next))
            }
            // The host can run the callee here: enter it **in this machine** with
            // the caller's return address stored in the live frame — exactly the
            // local `Instr::Call` discipline (C's `i_apply` is
            // `$FIXED_APPLY` + `$SAVE_CONTINUATION_POINTER($NEXT_INSTRUCTION)` +
            // `$DISPATCH_ABS`, `emu/instrs.tab:298-304`), so a `receive` inside the
            // callee suspends *this* process and its `return` unwinds into the
            // caller's frame.
            //
            // The stored continuation is the **caller's** next instruction
            // (`func`, `pc + 1` — this function's own position), which is why the
            // target's own `(func, pc)` are bound under different names.
            ApplyTarget::Enter {
                context,
                func: target_func,
                pc: target_pc,
            } => {
                machine.set_continuation(Continuation::Address {
                    func,
                    pc: pc + 1,
                    context: machine.current_context(),
                })?;
                Ok(Some(Step::Enter {
                    context,
                    func: target_func,
                    pc: target_pc,
                }))
            }
            // A non-atom target is the catchable `badarg` raise: the landing
            // already wrote x(0..=3), so x(0) must not be overwritten here.
            ApplyTarget::Raised(step) => Ok(Some(step)),
        },

        // `apply_last Arity Deallocate` — pop the frame, then apply and continue
        // through the machine's continuation (a tail call).
        //
        // **Pop timing (deliberate).** The pop precedes `apply_target`, matching
        // C's `i_apply_last`: `$APPLY` (`i_apply_last`) resolves the export and
        // `$deallocate` runs *before* the resolved callee executes, so a raise from
        // the callee (e.g. `erlang:error/1`) happens with this frame already gone
        // (`emu/instrs.tab:266-272`). Only C's own resolution failure
        // (`fixed_apply` returning NULL ⇒ `badarg`/`system_limit`) keeps the frame;
        // a raise there still lands correctly through the engine because the
        // handler of a `try` enclosing this tail call lives in an *outer* frame that
        // the pop does not touch — pinned by
        // `tests/call_ext_apply.rs::a_host_bif_error_from_a_tail_apply_last_lands_in_the_enclosing_try`.
        Instr::ApplyLast { arity, deallocate } => {
            machine.pop_frame_checked(*deallocate)?;
            match apply_target(machine, entry_module, module, *arity, ctx)? {
                ApplyTarget::Called(result) => {
                    machine.set_x(0, result)?;
                    // Follow the frame's continuation **context-aware**: this
                    // function may have been entered through the cross-module
                    // `resolve_external_call` seam, so its return address can name a
                    // *different* module. `continuation_step` is the one place that
                    // distinguishes a `Halt`, an intra-context `Goto` and a
                    // cross-context `Enter` (the discipline the `call_ext*`
                    // synchronous fallbacks in the interpreter already follow).
                    Ok(Some(machine.continuation_step()?))
                }
                // The frame is already popped (C's `apply_last` is `$FIXED_APPLY` +
                // `$deallocate` + `$DISPATCH_ABS`, `emu/instrs.tab:306-312`), so the
                // callee runs on the **caller's** frame and its `return` follows that
                // frame's (possibly cross-context) continuation — the tail-call
                // discipline the `call_ext_last` seam arm follows too.
                ApplyTarget::Enter { context, func, pc } => {
                    Ok(Some(Step::Enter { context, func, pc }))
                }
                ApplyTarget::Raised(step) => Ok(Some(step)),
            }
        }

        // Not this family's variant.
        _ => Ok(None),
    }
}

/// Collect the `arity` register arguments `x(0)..x(arity-1)`.
fn register_args(machine: &Machine, arity: u32) -> Result<Vec<Eterm>> {
    let mut args = Vec::with_capacity(arity as usize);
    for i in 0..arity {
        args.push(machine.x(i).ok_or_else(|| error::bad_register(i))?);
    }
    Ok(args)
}

/// The shared `call_fun`/`call_fun2` dispatch — the in-machine *closure-apply* seam.
///
/// The host is consulted **first** through
/// [`ExecutionContext::resolve_local_fun`]: a host that owns the closure's
/// defining module answers `Some((callee_module, lambda_entry_label))` and the
/// engine enters the lambda body **in this machine**, exactly as C's `call_fun`
/// (`../../erts/emulator/beam/beam_common.c:1604`) sets `reg[i + arity] = funp->env[i]`
/// and dispatches to the fun body's `code_ptr` in the same process:
///
/// * the call `args` are already in `x(0..arity)` (C's `reg[i] = args[i]`);
/// * the closure's free variables are copied into `x(arity..arity + num_free)`
///   (C's `reg[i + arity] = funp->env[i]` order — arguments first, captures after);
/// * the caller's next instruction (`func`, `pc + 1`) is stored as the live frame's
///   continuation (the non-tail discipline — `call_fun`/`call_fun2` are non-tail and
///   the loader emits no `call_fun_last`), and the lambda entry is entered as
///   [`Step::Enter`].
///
/// A `receive` inside the closure then suspends *this* process (an ordinary
/// [`crate::RunOutcome::Wait`]), and a raise inside it flows into the caller's live
/// `try`/`catch` frames rather than escaping as a typed error. When the host
/// declines (`Ok(None)` — the seam's default) the value-returning
/// [`ExecutionContext::apply_fun`] is the fallback, byte-identical to a host without
/// the seam.
///
/// Every error — the seam's own and the fallback's — is routed through the
/// catch-aware funnel [`Machine::raise_bif_error`], so a routable raise (a
/// `badarg`/`badarity` from a nested machine) lands in a live `try`/`catch` exactly
/// as C's `handle_error` → `next_catch` does; a genuine capability gap
/// ([`error::unsupported`], whose reason atom the running module cannot name) stays
/// the uncatchable typed error.
fn call_fun(
    machine: &mut Machine,
    module: &Module,
    fun: Eterm,
    args: &[Eterm],
    arity: u32,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    match ctx.resolve_local_fun(fun, args) {
        Ok(Some((callee, label))) => {
            // C's free-variable layout: the call arguments stay in `x(0..arity)`,
            // the closure's captured variables follow at `x(arity..)`. The `fun`
            // term's storage is read (never moved — no collection happens between
            // the read and the writes), so the copies are plain `Eterm` loads.
            let free = terms::fun_free(fun)?;
            for (i, value) in free.iter().enumerate() {
                let slot = arity + i as u32;
                machine.set_x(slot, *value)?;
            }
            let context = machine.register_context(&callee);
            let handle = machine
                .module_context(context)
                .ok_or_else(|| error::unsupported("call_fun: unknown module context"))?;
            let (target_func, target_pc) = handle.labels.resolve(label)?;
            // The caller's return address, stored in the live frame (the local
            // `Instr::Call` discipline; the same shape the `Instr::Apply` enter
            // branch uses).
            machine.set_continuation(Continuation::Address {
                func,
                pc: pc + 1,
                context: machine.current_context(),
            })?;
            Ok(Some(Step::Enter {
                context,
                func: target_func,
                pc: target_pc,
            }))
        }
        // The host cannot run this closure in-machine: the value-returning
        // fallback, unchanged for a host without the seam.
        Ok(None) => match ctx.apply_fun(fun, args) {
            Ok(result) => {
                machine.set_x(0, result)?;
                Ok(Some(Step::Next))
            }
            // A raise inside the applied closure must route through the
            // catch-aware funnel, not escape the caller's `try`/`catch`.
            Err(e) => Ok(Some(machine.raise_bif_error(module, e)?)),
        },
        Err(e) => Ok(Some(machine.raise_bif_error(module, e)?)),
    }
}

/// The outcome of an `apply`/`apply_last` target dispatch ([`apply_target`]).
pub(crate) enum ApplyTarget {
    /// The target was resolved to a `module:function` and dispatched; the applied
    /// result value is in hand (the caller stores it in `x(0)`).
    Called(Eterm),
    /// The host can run the target **in this machine** (the cross-module-call seam,
    /// [`ExecutionContext::resolve_external_call`]): `(context, func, pc)` is the
    /// callee's registered module context and its export entry. The instruction
    /// applies its own frame discipline and reports [`Step::Enter`].
    Enter { context: u32, func: u32, pc: u32 },
    /// The target dispatch **raised** instead of returning a value: either the
    /// target shape was invalid — a non-atom module/function — or the host BIF the
    /// target resolved to failed. Both are the C's catchable raise
    /// (`fixed_apply`'s `badarg`, and `$HANDLE_APPLY_ERROR` → `handle_error` →
    /// `next_catch` for a BIF failure, `emu/instrs.tab:234-296`), routed through
    /// the machine (see [`raise_badarg`] and
    /// [`Machine::raise_bif_error`](crate::machine::Machine::raise_bif_error)). The
    /// `Step` is the handler landing the raise produced (a live catch frame); the
    /// landed registers (`x(0) = THE_NON_VALUE`, `x(1..=3)`) must not be clobbered.
    Raised(Step),
}

/// Dispatch the `fixed_apply` shape: `module = x(arity)`, `function = x(arity+1)`,
/// arguments `x(0..arity-1)`.
///
/// Both the module and the function must be atoms (the C raises `badarg` with the
/// argument list otherwise — see [`raise_badarg`]). The target is then resolved
/// through the host's cross-module-call seam
/// ([`ExecutionContext::resolve_external_call`]) **first**: a host that owns the
/// callee's module runs it in this machine ([`ApplyTarget::Enter`]) so a `receive`
/// inside it suspends the *calling* process instead of folding into the typed
/// [`error::blocked`](crate::error::blocked) at the call site. This mirrors C,
/// where `fixed_apply` (`beam_common.c:1528`) resolves the export and jumps into
/// its code in the same process. When the host declines (`Ok(None)` — the seam's
/// default) the target is applied through
/// [`ExecutionContext::call_bif`](crate::ExecutionContext::call_bif); a **host BIF
/// error** is not allowed to escape the caller's live `try`/`catch` — it is routed
/// through [`Machine::raise_bif_error`](crate::machine::Machine::raise_bif_error)
/// (the C's `$HANDLE_APPLY_ERROR` → `handle_error` → `next_catch`) and reported as
/// [`ApplyTarget::Raised`], while a successful value is reported as
/// [`ApplyTarget::Called`] (the previous synchronous route, byte-identical). This
/// also covers the C's `apply of apply/3`
/// special case (`beam_common.c:1554-1557`): `x(0)`/`x(1)`/`x(2)` are then handed
/// straight to `erlang:apply/3` as its `M,F,Args` arguments, which is exactly what
/// the recursion into `apply()` computes.
pub(crate) fn apply_target(
    machine: &mut Machine,
    entry_module: &Module,
    module: &Module,
    arity: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<ApplyTarget> {
    let module_term = machine.x(arity).ok_or_else(|| error::bad_register(arity))?;
    let function_term = machine
        .x(arity + 1)
        .ok_or_else(|| error::bad_register(arity + 1))?;
    // `fixed_apply` (`beam_common.c:1539-1552`) checks the *function* first, then
    // the *module*; both a non-atom `module` and a non-atom `function` set
    // `p->freason = BADARG` and return NULL into `handle_error` → `next_catch`, so
    // the `badarg` is a **catchable raise**, not a bare return.
    let (module_name, function_name) = match (
        atom_name(module, module_term),
        atom_name(module, function_term),
    ) {
        (Ok(module_name), Ok(function_name)) => (module_name, function_name),
        _ => return raise_badarg(machine, module).map(ApplyTarget::Raised),
    };
    let args = register_args(machine, arity)?;
    // The triple handed to the host is exactly the one `call_bif` receives (the
    // applied `module:function/arity`), so a host answers the seam however it
    // answers the BIF dispatch. A resolved callee is registered as a module
    // context and its entry `Label` resolved against the callee's **own** label
    // index — an unknown label is a typed error, never a jump into the wrong
    // module.
    if let Some((callee, label)) = ctx.resolve_external_call(module_name, function_name, arity)? {
        let context = machine.register_context(&callee);
        let handle = machine
            .module_context(context)
            .ok_or_else(|| error::unsupported("apply: unknown module context"))?;
        let (func, pc) = handle.labels.resolve(label)?;
        return Ok(ApplyTarget::Enter { context, func, pc });
    }
    // The host could not run the callee in-machine: dispatch it through the
    // synchronous BIF path. A **host BIF error** is not allowed to escape the
    // caller's live `try`/`catch`: C routes every apply failure through
    // `$HANDLE_APPLY_ERROR` → `handle_error` → `next_catch`
    // (`../../erts/emulator/beam/emu/instrs.tab:234-296`,
    // `../../erts/emulator/beam/beam_common.c:1539-1557`), so the raise is
    // catchable. [`Machine::raise_bif_error`] implements exactly that contract
    // (a routable raise lands in the innermost live catch frame; a genuine
    // capability gap — whose reason atom the running module cannot name — stays
    // the uncatchable typed `Err`, and with no catch frame the error propagates
    // unchanged), so the single call here covers the `apply`/`apply_last` arms
    // **and** the `call_ext erlang:apply/3` seam route (both consume
    // [`ApplyTarget`]). Engine-diagnostic `Err`s from *outside* the host BIF call
    // (an out-of-range register, the resolver / label resolution above) keep their
    // `?` and are never wrapped — only the host's own BIF outcome is.
    //
    // An unresolvable target raises `error:undef`, and the oracle's cooked trace
    // then names the **callee** with its argument list as the innermost item (C's
    // `erts_build_mfa_item` with the exception's saved args — see
    // [`callee`]). This site knows the callee (the two atoms checked above, both
    // immediate terms that cannot move) and its arguments are the instruction's own
    // `x(0..arity)` registers, so the item is synthesized here: a no-op for any
    // other error shape and when the raise cannot land.
    match machine.dispatch_bif_call(
        entry_module,
        module_name,
        function_name,
        arity,
        &args,
        ctx,
    ) {
        Ok(value) => Ok(ApplyTarget::Called(value)),
        Err(e) => {
            let head = callee::callee_head_item(machine, ctx, &e, module_term, function_term, arity)?;
            Ok(ApplyTarget::Raised(
                machine.raise_bif_error_with_head(module, e, head)?,
            ))
        }
    }
}

/// Raise the `badarg` semantic error through the machine, landing it in a live
/// catch frame (or a typed [`BeamError::BadArg`] when there is none).
///
/// C's `fixed_apply` (`beam_common.c:1539-1552`) sets `p->freason = BADARG` and
/// returns NULL into `handle_error` → `next_catch`
/// (`../../erts/emulator/beam/beam_common.c:580-679`), so the raise **is
/// catchable** by an enclosing `try`/`catch` (verified against the oracle: `try
/// apply(1, f, []) catch C:R -> {C,R} end` yields `{badarg,...}` with class
/// `error`). The engine's raise/unwind entry point is [`Machine::throw_error`],
/// with the reason atom `badarg` resolved from the running module's atom table
/// (the engine never invents an atom the module cannot name).
///
/// A module whose atom table lacks `badarg` cannot name the reason, so the typed
/// [`BeamError::BadArg`] is returned instead — never a substituted, wrong atom.
/// With **no live catch frame** the raise cannot land, so it likewise propagates
/// unchanged as the typed [`BeamError::BadArg`] (the same uncaught-error contract
/// the interpreter's `raise_bif_error` follows). The `error` class atom is
/// installed when the module has it, which is what a landing places in `x(3)`.
pub(crate) fn raise_badarg(machine: &mut Machine, module: &Module) -> Result<Step> {
    if machine.catch_frame_depth() == 0 {
        return Err(BeamError::BadArg);
    }
    let atom = module
        .atom_table
        .lookup("badarg")
        .ok_or(BeamError::BadArg)?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(make_atom(atom))
}

/// The name of an atom term, resolved through the running module's atom table.
///
/// A non-atom term, or an atom index absent from the table, is a typed
/// [`BeamError::BadArg`] (the C's `BADARG`); the engine never substitutes a
/// *different* atom.
fn atom_name<'a>(module: &'a Module, term: Eterm) -> Result<&'a str> {
    if !is_atom(term) {
        return Err(BeamError::BadArg);
    }
    module.atom_name(atom_val(term)).ok_or(BeamError::BadArg)
}

#[cfg(test)]
mod tests;
