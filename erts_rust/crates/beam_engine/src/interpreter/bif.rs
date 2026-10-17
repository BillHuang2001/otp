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

use beam_common::{BeamError, ExceptionClass, Result, StackTrace};
use beam_loader::{ImportRef, Label, Module, Operand, NO_FAIL};
use beam_term::{make_atom, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::{namespace_atom, Machine};use crate::operand::{read_operand, write_operand};
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
        entry_module: &Module,
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
            self.dispatch_bif_call(
                entry_module,
                module_name,
                function_name,
                arity,
                &buf[..args.len()],
                ctx,
            )
        } else {
            // `?` propagates a *marshalling* error (a bad operand) as the typed error
            // the instruction already returned — never as a guard failure or a raise.
            self.dispatch_wide(
                entry_module,
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
            Err(e) if fail == NO_FAIL => self.raise_bif_error(module, e, ctx),
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
        entry_module: &Module,
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
            Ok(()) => Ok(self.dispatch_bif_call(entry_module, module, function, arity, &scratch, ctx)),
            Err(e) => Err(e),
        };
        // Give the (now empty) buffer back so the next wide call reuses it.
        scratch.clear();
        self.args_scratch = scratch;
        outcome
    }

    /// Call an imported BIF with the `arity` register arguments `x(0..arity)`.
    ///
    /// The import's `(module, function, arity)` comes from the per-run cache
    /// ([`LabelMap::import_target`], `labels`) and the marshalling is
    /// [`Machine::call_bif_named`]'s. Note the two arities it passes on: the
    /// **registers** are the *instruction's* `arity` operand while the BIF is
    /// dispatched with the **import table's** declared arity — the split an
    /// `call_ext N erlang:apply/3`-shaped import needs, preserved verbatim by the
    /// shared helper. A marshalling error (a bad import or an out-of-range register)
    /// is handed back as an `Err` like a BIF failure — which the caller raises,
    /// exactly as before this seam.
    pub(super) fn call_import(
        &mut self,
        entry_module: &Module,
        module: &Module,
        labels: &LabelMap,
        target: ImportRef,
        arity: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let (module_name, function_name, bif_arity) = labels.import_target(module, target)?;
        self.call_bif_named(
            entry_module,
            module_name,
            function_name,
            bif_arity,
            arity,
            ctx,
        )
    }

    /// Marshal the register arguments `x(0..nregs)` and dispatch
    /// `module_name:function_name/bif_arity` through [`ExecutionContext::call_bif`].
    ///
    /// The **one** definition of the register-marshalling shape every BIF-dispatching
    /// seam shares — a `call_ext*` ([`Machine::call_import`]) and the *local-call*
    /// seam's synchronous route (the three local call arms, which serve a same-module
    /// callee through this helper when the host's
    /// [`ExecutionContext::resolve_local_call`] answers `true`).
    ///
    /// The two arities are deliberately separate: `nregs` is how many register
    /// arguments to marshal (the instruction's own `arity` operand) while `bif_arity`
    /// is what the BIF is dispatched with (the import table's declared arity). For a
    /// well-formed call they are equal; a `call_ext 3 erlang:apply/3`-shaped import is
    /// where they differ — which is exactly the distinction the local-call route must
    /// not lose.
    ///
    /// Like [`Machine::exec_bif`] the arguments are marshalled through a **stack
    /// array** for the common arities ([`STACK_ARGS`]) and through the machine's
    /// reusable scratch buffer beyond it, so no arity allocates a heap block per call
    /// (after the first wide one). A marshalling error (an out-of-range register) is an
    /// `Err`, exactly as the call sites expect.
    pub(crate) fn call_bif_named(
        &mut self,
        entry_module: &Module,
        module_name: &str,
        function_name: &str,
        bif_arity: u32,
        nregs: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        if nregs as usize <= STACK_ARGS {
            let mut buf = EMPTY_ARGS;
            for (i, slot) in buf.iter_mut().enumerate().take(nregs as usize) {
                *slot = self
                    .x(i as u32)
                    .ok_or_else(|| error::bad_register(i as u32))?;
            }
            self.dispatch_bif_call(
                entry_module,
                module_name,
                function_name,
                bif_arity,
                &buf[..nregs as usize],
                ctx,
            )
        } else {
            // A marshalling error and a BIF failure are the same `Err` to this
            // function's callers (both are raised), so the two are flattened.
            self.dispatch_wide(
                entry_module,
                module_name,
                function_name,
                bif_arity,
                nregs as usize,
                ctx,
                |machine, _ctx, scratch| {
                    for i in 0..nregs {
                        scratch.push(machine.x(i).ok_or_else(|| error::bad_register(i))?);
                    }
                    Ok(())
                },
            )
            .and_then(|result| result)
        }
    }

    /// Dispatch a BIF through the host, taking the **in-machine** door when the
    /// host asked for it ([`ExecutionContext::wants_bif_in_machine`](crate::context::ExecutionContext::wants_bif_in_machine),
    /// latched into [`Machine::bif_in_machine_on`](crate::machine::Machine) once per
    /// run by whichever driver owns the run — [`Machine::drive`](crate::machine::Machine::drive)
    /// for the interpreter and the native baseline tier's own driver for
    /// [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit)).
    ///
    /// This is the **single** BIF-dispatch funnel the engine's marshalling helpers
    /// call, so the two doors can never drift:
    ///
    /// * gate **off** (the default, every existing host): the plain
    ///   [`ExecutionContext::call_bif`](crate::context::ExecutionContext::call_bif)
    ///   with the same arguments — byte-identical to the pre-seam engine, one `bool`
    ///   load and no extra vtable crossing;
    /// * gate **on**: [`ExecutionContext::call_bif_in_machine`](crate::context::ExecutionContext::call_bif_in_machine)
    ///   with `self` (the live machine) and `entry_module` (the run's entry module,
    ///   which the host needs to name a context-`0` frame) handed in alongside the
    ///   same `module`/`function`/`arity`/`args`.
    ///
    /// `entry_module` is the module the run was entered at — the same one
    /// [`Machine::drive`](crate::machine::Machine::drive) threads down from its own
    /// entry module argument. It is unused when the gate is off.
    #[inline]
    pub(crate) fn dispatch_bif_call(
        &self,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        if self.bif_in_machine_on {
            ctx.call_bif_in_machine(self, entry_module, module, function, arity, args)
        } else {
            ctx.call_bif(module, function, arity, args)
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
    ///   `beam_bifs`-backed no-clause failure raise). The reason atom — and the
    ///   class atom a landing installs in `x(3)` — is resolved through the engine's
    ///   **live atom namespace** with the running module's load-time snapshot as the
    ///   fallback ([`namespace_atom`]), so a reason the runtime interned after the
    ///   module was loaded still lands in the handler.
    /// * An error the engine cannot name as a term propagates unchanged instead: a
    ///   [`BeamError::Raise`] whose reason is
    ///   [`beam_common::ExitReason::Structured`] **without** a live reason slot, a
    ///   variant with no BEAM atom ([`BeamError::TermNotFound`], [`BeamError::Io`]),
    ///   or an atom **neither** the live namespace nor the module's atom table
    ///   holds. Substituting a *different* atom would be a wrong result, so the
    ///   caller's typed error is preferred (the engine never invents an atom index,
    ///   because an index is valid only against the table it was read with).    ///
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
    /// A [`BeamError::Raise`] whose [`stacktrace`](beam_common::Raise::stacktrace) is
    /// **non-empty** arrived carrying explicit symbolic frames (e.g. an
    /// `erlang:raise/3` whose third argument is a trace list). They are armed on the
    /// one-shot `pending_supplied_trace` latch immediately before the landing
    /// `throw_with_trace`, so the caught-path cooked trace renders THOSE frames in
    /// preference to the `raise_snapshot` heuristic (see
    /// [`crate::families::exceptions::stacktrace::symbolic_stacktrace`]). An
    /// absent/empty supplied trace arms nothing and leaves every path byte-identical.
    ///
    /// Shared with the instruction families (e.g. `families::receive`'s `send`,
    /// which carries no fail label either), so the landing convention stays defined
    /// in exactly one place.
    pub(crate) fn raise_bif_error(
        &mut self,
        module: &Module,
        error: BeamError,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {
        self.raise_error(module, error, None, ctx)
    }
    /// [`Machine::raise_bif_error`] with a synthesized **callee head item**.
    ///
    /// `head` is the `{CalleeModule, CalleeFunction, ArgList, []}` item a failing
    /// `call_ext*`/`apply` site built for an unresolvable target (see
    /// [`crate::families::exceptions::callee`]); it is raised as the exception's own
    /// **raw trace** (C's saved trace object, which is what `build_stacktrace`
    /// resolves) and the one-shot `pending_callee_head` latch is armed **immediately
    /// before** the `throw_with_trace` that actually lands the raise — never at the
    /// call site, because a raise with **no live catch frame** propagates as a bare
    /// `Err` without ever reaching `throw_with_trace` (the head is then discarded and
    /// the latch must stay clear for the next, unrelated raise).
    ///
    /// `None` is byte-identical to [`Machine::raise_bif_error`].
    pub(crate) fn raise_bif_error_with_head(
        &mut self,
        module: &Module,
        error: BeamError,
        head: Option<Eterm>,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {
        self.raise_error(module, error, head, ctx)
    }

    /// The shared body of [`Machine::raise_bif_error`]/
    /// [`Machine::raise_bif_error_with_head`] (see the former for the full contract).
    ///
    /// Both the **reason** atom and the **class** atom are resolved through the
    /// engine's live atom namespace
    /// ([`crate::machine::namespace_atom`]): a name the host's live namespace    /// knows is nameable even when the running module's load-time snapshot is not
    /// (the runtime interns `badarg`/`error`/… into its shared table, and a module
    /// loaded earlier cannot see them). The default
    /// [`ExecutionContext::atoms`](crate::context::ExecutionContext::atoms) body is
    /// the module snapshot, so every host that does not override the seam behaves
    /// exactly as before.
    fn raise_error(
        &mut self,
        module: &Module,
        error: BeamError,
        head: Option<Eterm>,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {        // A `None` head leaves every path byte-identical to the pre-head funnel: the
        // raw trace stays `NIL` and the latch is never armed.
        let has_head = head.is_some();
        let trace = head.unwrap_or(NIL);
        // The symbolic frames an incoming raise **arrived carrying** (e.g.
        // `erlang:raise/3` with a non-empty trace list). Extracted — never emptied —
        // only when non-empty, so an absent/empty supplied trace leaves every path
        // byte-identical to the `raise_snapshot`-derived behavior.
        let supplied_trace = match &error {
            BeamError::Raise(raise) if !raise.stacktrace.is_empty() => Some(raise.stacktrace.clone()),
            _ => None,
        };
        // A raise whose reason is a compound term lands (or propagates) with the
        // term, mirroring C's `handle_error` → `next_catch` for every reason shape.
        if let BeamError::Raise(raise) = &error {
            if let Some(reason) = raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>())
            {
                let class = raise.class;
                let reason = *reason;
                if let Some(class_atom) = namespace_atom(ctx, module, class.as_str()) {
                    self.set_class_atom(class, make_atom(class_atom));
                }                return self.throw_with_trace_headed(
                    class,
                    reason,
                    trace,
                    has_head,
                    supplied_trace,
                );
            }
        }
        // No live catch frame: the error propagates unchanged, carrying whatever
        // class/reason (and live reason term) it already has. The head is dropped
        // with it — no `throw_with_trace` runs, so the latch stays clear.
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
        let Some(atom) = namespace_atom(ctx, module, name) else {
            return Err(error);
        };
        // The class atom a landing installs in `x(3)`; a class **neither** the live
        // namespace nor the module snapshot can name leaves it unset and
        // `throw_with_trace` reports its own typed error rather than substituting a
        // wrong atom.
        if let Some(class_atom) = namespace_atom(ctx, module, class.as_str()) {
            self.set_class_atom(class, make_atom(class_atom));
        }        self.throw_with_trace_headed(class, make_atom(atom), trace, has_head, supplied_trace)
    }

    /// [`Machine::throw_with_trace`] that first arms the one-shot latches the raise
    /// arrived with, so the raise being landed is marked as carrying a synthesized
    /// callee head item (when `has_head`, which lives in `trace`) and/or the explicit
    /// symbolic frames it arrived with (when `supplied_trace` is `Some`).
    #[inline]
    fn throw_with_trace_headed(
        &mut self,
        class: ExceptionClass,
        reason: Eterm,
        trace: Eterm,
        has_head: bool,
        supplied_trace: Option<StackTrace>,
    ) -> Result<Step> {
        if has_head {
            self.pending_callee_head = true;
        }
        // The supplied frames are armed on the same one-shot latch the landing
        // `throw_with_trace` consumes. Only a non-empty trace arms it: an absent or
        // empty supplied trace leaves the latch clear, so the caught-path cooked
        // trace stays the `raise_snapshot`-derived one.
        if supplied_trace.is_some() {
            self.pending_supplied_trace = supplied_trace;
        }
        self.throw_with_trace(class, reason, trace)
    }
}

#[cfg(test)]
mod tests {
    //! The raise funnel's **atom-namespace** contract: a catchable raise must land in
    //! a live catch frame even when the `x(1)` reason atom and the `x(3)` class atom
    //! are **not** in the running module's load-time snapshot (the runtime interns them
    //! into its one shared table, and a module loaded earlier cannot see them).
    use super::*;

    use beam_common::ExceptionClass;
    use beam_term::{make_small, AtomTable, THE_NON_VALUE};

    use crate::context::MinimalContext;
    use crate::machine::CatchFrame;

    /// A machine parked under a live catch frame that lands at `(func, pc)`.
    fn machine_with_a_catch_frame(func: u32, pc: u32) -> Machine {
        let mut machine = Machine::new();
        machine.push_frame(0).expect("the bottom frame");
        machine.push_catch_frame(CatchFrame::new(func, pc, 0));
        machine
    }

    /// `module` is an atom-poor synthetic module (`Module::default()` interns nothing);
    /// `namespace` is the engine's **live** shared table — the module's snapshot plus
    /// the names the runtime interned after it was loaded.
    fn namespace_with(module: &Module, names: &[&str]) -> AtomTable {
        let mut live = module.atom_table.clone();
        for name in names {
            live.intern(name);
        }
        live
    }

    #[test]
    fn a_bare_badarg_raise_lands_through_the_live_namespace() {
        // W2: the module cannot name `badarg` OR `error`; the host's namespace can.
        // Before the fix the reason-atom lookup was snapshot-only, so the raise
        // escaped a live catch frame as a hard `Err`.
        let module = Module::default();
        assert!(module.atom_table.lookup("badarg").is_none());
        assert!(module.atom_table.lookup("error").is_none());
        let live = namespace_with(&module, &["badarg", "error"]);
        let badarg = live.lookup("badarg").expect("the namespace names badarg");
        let error = live.lookup("error").expect("the namespace names error");
        assert!(module.atom_table.lookup("badarg").is_none(), "beyond the snapshot");

        let mut machine = machine_with_a_catch_frame(4, 7);
        let mut ctx = MinimalContext::with_shared_atoms(live);

        let step = machine
            .raise_bif_error(&module, BeamError::BadArg, &mut ctx)
            .expect("a live catch frame must catch the raise");
        assert_eq!(step, Step::Goto { func: 4, pc: 7 });
        // The C landing convention: x(0) = THE_NON_VALUE, x(1) = the reason term,
        // x(3) = the class atom — both resolved from the live namespace.
        assert_eq!(machine.x(0), Some(THE_NON_VALUE));
        assert_eq!(machine.x(1), Some(make_atom(badarg)));
        assert_eq!(machine.x(3), Some(make_atom(error)));
        assert_eq!(machine.catch_frame_depth(), 1, "the handler pops it");
    }

    #[test]
    fn a_bare_badarg_raise_whose_namespace_cannot_name_it_still_propagates() {
        // The negative half: a namespace that cannot name the reason leaves the raise
        // untouched (the typed error), never a substituted atom.
        let module = Module::default();
        let live = namespace_with(&module, &["error"]);
        let mut machine = machine_with_a_catch_frame(4, 7);
        let mut ctx = MinimalContext::with_shared_atoms(live);

        let err = machine
            .raise_bif_error(&module, BeamError::BadArg, &mut ctx)
            .expect_err("an unnameable reason cannot land");
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(machine.catch_frame_depth(), 1, "the frame is left untouched");
    }

    #[test]
    fn an_equivalent_raise_is_unchanged_without_a_shared_namespace() {
        // The default `ExecutionContext::atoms` body IS the module snapshot, so a host
        // that installs no namespace keeps the pre-seam behaviour exactly: a module
        // that names the atoms lands, and one that does not propagates the typed error.
        let mut module = Module::default();
        let badarg = module.atom_table.intern("badarg");
        module.atom_table.intern("error");
        let mut machine = machine_with_a_catch_frame(1, 2);
        let mut ctx = MinimalContext::new();

        let step = machine
            .raise_bif_error(&module, BeamError::BadArg, &mut ctx)
            .expect("the module's own snapshot names the atoms");
        assert_eq!(step, Step::Goto { func: 1, pc: 2 });
        assert_eq!(machine.x(1), Some(make_atom(badarg)));

        let bare = Module::default();
        let mut other = machine_with_a_catch_frame(1, 2);
        let err = other
            .raise_bif_error(&bare, BeamError::BadArg, &mut ctx)
            .expect_err("no namespace, no name");
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn the_reason_term_raise_installs_the_class_atom_through_the_namespace() {
        // The live-reason (compound reason) branch resolves its class atom the same
        // way, so `{badmap,_}`-shaped raises land in an atom-poor module too.
        let module = Module::default();
        let live = namespace_with(&module, &["error"]);
        let error = live.lookup("error").expect("the namespace names error");
        let mut machine = machine_with_a_catch_frame(0, 9);
        let mut ctx = MinimalContext::with_shared_atoms(live);
        let mut reason_ctx = MinimalContext::new();
        let reason = crate::terms::make_tuple(&mut reason_ctx, &[make_small(1)]).unwrap();

        let err = BeamError::Raise(
            beam_common::Raise::new(ExceptionClass::Error, beam_common::ExitReason::Structured)
                .with_live_reason(beam_common::LiveReason::new(reason)),
        );
        let step = machine
            .raise_bif_error(&module, err, &mut ctx)
            .expect("a live catch frame must catch the term-carrying raise");
        assert_eq!(step, Step::Goto { func: 0, pc: 9 });
        assert_eq!(machine.x(1), Some(reason), "the compound reason lands verbatim");
        assert_eq!(machine.x(3), Some(make_atom(error)));
    }
}