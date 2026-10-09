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

//! BIF-argument marshalling and the BIF error/raise path (`impl Machine`).
//!
//! `exec_bif`/`call_import` marshal a `gc_bif`/`bif`/`call_ext*`'s arguments and
//! call the BIF through [`ExecutionContext::call_bif`]; `raise_bif_error` folds a
//! BIF error into the catch-aware raise the dispatch loop expects.

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{ImportRef, Label, Module, Operand, NO_FAIL};
use beam_term::{make_atom, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::{resolve, LabelMap};

/// The argument count up to which a BIF/`call_ext` marshals its arguments through
/// a **stack array** instead of the machine's reusable scratch buffer.
///
/// `gc_bif`/`bif`/`call_ext` used to build a fresh `Vec` (one heap allocation and
/// free) for **every** BIF call, which a micro-benchmark showed to be the single
/// largest per-instruction cost of the interpreter. arities in `0..=STACK_ARGS`
/// (every arity the compiler actually emits for arithmetic/guard BIFs) now fill a
/// fixed stack array — no `Vec` is even constructed, let alone allocated — and a
/// larger arity reuses [`Machine::args_scratch`], so **no** arity allocates per
/// call. The observable behaviour and the `ctx.call_bif(.., &[Eterm])` signature
/// are unchanged.
pub(crate) const STACK_ARGS: usize = 2;

/// A stack array sized for the common BIF/`call_ext` arities ([`STACK_ARGS`]).
const EMPTY_ARGS: [Eterm; STACK_ARGS] = [NIL; STACK_ARGS];

impl Machine {
    /// Read a `gc_bif`/`bif`/`bif0`'s **operand** arguments, call the BIF through
    /// [`ExecutionContext::call_bif`] and fold the outcome into the control-flow
    /// [`Step`] the dispatch loop expects.
    ///
    /// The arguments are marshalled into a **stack array** for the common arities
    /// ([`STACK_ARGS`]) and into a heap `Vec` only beyond it, so the heap
    /// allocation the old code paid on **every** BIF call is gone; `call_bif` keeps
    /// its `&[Eterm]` shape either way. The BIF's `(module, function, arity)`
    /// target comes from the per-run import cache ([`LabelMap::import_target`]),
    /// which replaced the two per-call atom-table lookups the interpreter used to
    /// pay here (measured: ~5 ns of the ~20 ns a small-int `gc_bif '+'/2` costs).
    ///
    /// The control flow is unchanged, and the distinction matters: an error while
    /// *marshalling* the call (a bad import reference or a bad operand) propagates
    /// as the typed error the instruction already returned (the original `?`),
    /// **never** as a guard failure — only a **BIF** error raises
    /// ([`Machine::raise_bif_error`], which is the whole story when `fail == NO_FAIL`,
    /// i.e. for every `bif0`) or branches to `fail`.
    pub(super) fn exec_bif(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        bif: ImportRef,
        args: &[Operand],
        fail: Label,
        dst: &Operand,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {
        let (module_name, function_name, arity) = labels.import_target(module, bif)?;
        // The common arities marshal through a **stack array** (no `Vec`, no heap
        // block); only a wider call touches the machine's reusable scratch buffer,
        // and that one allocates nothing after the first such call.
        let bif_result: Result<Eterm> = if args.len() <= STACK_ARGS {
            let mut buf = EMPTY_ARGS;
            for (slot, arg) in buf.iter_mut().zip(args) {
                *slot = read_operand(self, ctx, arg)?;
            }
            ctx.call_bif(module_name, function_name, arity, &buf[..args.len()])
        } else {
            // `?` propagates a *marshalling* error (a bad operand) as the typed error
            // the instruction already returned — never as a guard failure or a raise.
            self.dispatch_wide(
                module_name,
                function_name,
                arity,
                args.len(),
                ctx,
                |machine, ctx, scratch| {
                    for arg in args {
                        scratch.push(read_operand(machine, ctx, arg)?);
                    }
                    Ok(())
                },
            )?
        };
        match bif_result {
            Ok(value) => {
                write_operand(self, dst, value)?;
                Ok(Step::Next)
            }
            // No fail label ⇒ a *raise*: land in a live catch frame (C falls
            // through to `find_func_info`/`handle_error`) or propagate the typed
            // error.
            Err(e) if fail == NO_FAIL => self.raise_bif_error(module, e),
            // Guard failure: jump to the fail label.
            Err(_) => {
                let (func, pc) = resolve(labels, fail)?;
                Ok(Step::Goto { func, pc })
            }
        }
    }

    /// Marshal a **wide** argument list (more than [`STACK_ARGS`] arguments) into
    /// the machine's reusable scratch buffer and dispatch it.
    ///
    /// The buffer is taken out of the machine (so the operand reader can borrow the
    /// machine immutably while the terms are pushed), cleared, filled and given
    /// back **empty** — only its capacity survives, which is exactly the point: a
    /// call of any arity allocates nothing once the buffer has grown to fit it.
    ///
    /// The return shape mirrors the stack-array path exactly: the **outer**
    /// [`Result`] is the *marshalling* outcome (a bad operand, propagated by the
    /// caller's `?` and never folded into a guard/raise decision) and the **inner**
    /// [`Result`] is the BIF's own outcome.
    fn dispatch_wide(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        len: usize,
        ctx: &mut dyn ExecutionContext,
        fill: impl FnOnce(&mut Machine, &mut dyn ExecutionContext, &mut Vec<Eterm>) -> Result<()>,
    ) -> Result<Result<Eterm>> {
        let mut scratch = std::mem::take(&mut self.args_scratch);
        scratch.clear();
        scratch.reserve(len);
        let marshalled = fill(self, &mut *ctx, &mut scratch);
        let outcome = match marshalled {
            Ok(()) => Ok(ctx.call_bif(module, function, arity, &scratch)),
            Err(e) => Err(e),
        };
        // Give the (now empty) buffer back so the next wide call reuses it.
        scratch.clear();
        self.args_scratch = scratch;
        outcome
    }

    /// Call an imported BIF with the `arity` register arguments `x(0..arity)`.
    ///
    /// Like [`Machine::exec_bif`] the arguments are marshalled through a stack
    /// array for the common arities and through the machine's reusable scratch
    /// buffer beyond it, so a `call_ext` pays no heap allocation in either case;
    /// the import target comes from the same per-run cache
    /// ([`LabelMap::import_target`], `labels`). A marshalling error (a bad import or
    /// an out-of-range register) is handed back as an `Err` like a BIF failure —
    /// which the caller raises, exactly as before this seam.
    pub(super) fn call_import(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        target: ImportRef,
        arity: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let (module_name, function_name, bif_arity) = labels.import_target(module, target)?;
        if arity as usize <= STACK_ARGS {
            let mut buf = EMPTY_ARGS;
            for (i, slot) in buf.iter_mut().enumerate().take(arity as usize) {
                *slot = self
                    .x(i as u32)
                    .ok_or_else(|| error::bad_register(i as u32))?;
            }
            ctx.call_bif(
                module_name,
                function_name,
                bif_arity,
                &buf[..arity as usize],
            )
        } else {
            // A marshalling error and a BIF failure are the same `Err` to this
            // function's callers (both are raised), so the two are flattened.
            self.dispatch_wide(
                module_name,
                function_name,
                bif_arity,
                arity as usize,
                ctx,
                |machine, _ctx, scratch| {
                    for i in 0..arity {
                        scratch.push(machine.x(i).ok_or_else(|| error::bad_register(i))?);
                    }
                    Ok(())
                },
            )
            .and_then(|result| result)
        }
    }

    /// Raise a BIF/`call_ext` error on the **no-fail-label** path, landing it in a
    /// live catch frame when there is one.
    ///
    /// Reproduces the C interpreter's error path for a BIF call without a fail
    /// label: `gc_bif`/`bif` fall through to `find_func_info`
    /// (`../../erts/emulator/beam/emu/macros.tab:266-278`) and `call_light_bif*`
    /// runs `handle_error` (`emu/bif_instrs.tab:308-314`, `:409-415`); both end in
    /// `next_catch` (`../../erts/emulator/beam/beam_common.c:580-679`), which finds
    /// the innermost live `try`/`catch` handler and continues there. The engine's
    /// single equivalent entry point is [`Machine::throw_with_trace`].
    ///
    /// * **No live catch frame** ([`Machine::catch_frame_depth`] `== 0`) ⇒ the error
    ///   propagates unchanged (whether it carries a live reason term or not).
    /// * **A live catch frame** ⇒ the exception is landed in the handler. Its reason
    ///   atom and class come from the error itself: a reason variant
    ///   (`badarg`/`badarith`/`badarity`/`badmatch`/`system_limit`/`out_of_memory`)
    ///   is raised with class `error` — the class every BIF failure carries
    ///   (`BADARG`/`BADARITH`/… are `EXTAG_ERROR` reasons) — while a
    ///   [`BeamError::Raise`] whose reason **is an atom** keeps its own class
    ///   (`error`/`exit`/`throw`: the shape `erlang:throw/1`/`exit/1` and every
    ///   `beam_bifs`-backed no-clause failure raise). The atom is resolved in the
    ///   running module's atom table and turned into the `x(1)` reason term.
    /// * An error the engine cannot name as a term propagates unchanged instead: a
    ///   [`BeamError::Raise`] whose reason is
    ///   [`beam_common::ExitReason::Structured`] **without** a live reason slot, a
    ///   variant with no BEAM atom ([`BeamError::TermNotFound`], [`BeamError::Io`]),
    ///   or an atom the running module's atom table does not hold. Substituting a
    ///   *different* atom would be a wrong result, so the caller's typed error is
    ///   preferred — a documented Milestone-1 limit (the [`Module`] reaches the
    ///   interpreter read-only, so a name the compiler never emitted cannot be
    ///   interned here, unlike the C runtime's global atom table).
    ///
    /// A [`BeamError::Raise`] that carries a live reason term
    /// ([`beam_common::Raise::live_reason`], payload [`Eterm`]) is an exception
    /// whose reason is a compound term — the shape [`beam_bifs`]-backed BIFs raise
    /// (e.g. `{badmap,M}`/`{badkey,K}`/`{bad_generator,G}`). It is raised through
    /// the one uniform entry point [`Machine::throw_with_trace`], exactly like an
    /// instruction-level raise: with a live catch frame it lands in the handler
    /// (`x(1) = reason`), and with none it propagates the term-carrying
    /// `Err(BeamError::Raise(..))`. This reproduces C, where `handle_error` always
    /// runs `next_catch` regardless of the reason's shape.
    ///
    /// Shared with the instruction families (e.g. `families::receive`'s `send`,
    /// which carries no fail label either), so the landing convention stays defined
    /// in exactly one place.
    pub(crate) fn raise_bif_error(&mut self, module: &Module, error: BeamError) -> Result<Step> {
        // A raise whose reason is a compound term lands (or propagates) with the
        // term, mirroring C's `handle_error` → `next_catch` for every reason shape.
        if let BeamError::Raise(raise) = &error {
            if let Some(reason) = raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>())
            {
                let class = raise.class;
                let reason = *reason;
                if let Some(class_atom) = module.atom_table.lookup(class.as_str()) {
                    self.set_class_atom(class, make_atom(class_atom));
                }
                return self.throw_with_trace(class, reason, NIL);
            }
        }
        // No live catch frame: the error propagates unchanged, carrying whatever
        // class/reason (and live reason term) it already has.
        if self.catch_frame_depth() == 0 {
            return Err(error);
        }
        // A live catch frame: the exception must be **nameable as a term** to land
        // in `x(1)`, so its reason atom and class come from the error itself. A
        // [`BeamError::Raise`] keeps its OWN class — a `throw`/`exit` whose reason
        // is a bare atom (the shape `erlang:throw/1`/`exit/1` and every
        // `beam_bifs`-backed no-clause failure raise) is a first-class raise and
        // must land in the handler exactly like a term-reason raise, never escape
        // because the reason happens to have no live *term* slot. A reason variant
        // (`badarg`/`badarith`/…) is always class `error`.
        let (class, name) = match &error {
            BeamError::Raise(raise) => (raise.class, raise.reason.as_atom()),
            other => (ExceptionClass::Error, other.as_atom()),
        };
        let Some(name) = name else {
            return Err(error);
        };
        let Some(atom) = module.atom_table.lookup(name) else {
            return Err(error);
        };
        // The class atom a landing installs in `x(3)`; a module that cannot name
        // the class leaves it unset and `throw_with_trace` reports its own typed
        // error rather than substituting a wrong atom.
        if let Some(class_atom) = module.atom_table.lookup(class.as_str()) {
            self.set_class_atom(class, make_atom(class_atom));
        }
        self.throw_with_trace(class, make_atom(atom), NIL)
    }
}
