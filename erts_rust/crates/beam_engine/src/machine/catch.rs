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

//! The exception scaffolding: the catch-frame stack and the raise/unwind entries.
//!
//! `try`/`catch` push a [`CatchFrame`]; a raise consults the innermost one and
//! either lands the exception in the handler registers (`x(0..=3)`, the C
//! `handle_error` convention) or propagates as a typed
//! [`BeamError::Raise`]. [`Machine::throw_with_trace`] is the **one uniform
//! raise/unwind entry point** every instruction family calls, and
//! [`Machine::with_uncaught_frames`] is the funnel that attaches an *uncaught*
//! raise's symbolic [`StackTrace`] (derived by [`Machine::raise_stacktrace`])
//! as it leaves the machine — so **every** raise kind gets the same frames
//! (`crate::families::exceptions` owns the handler opcodes that pop the frame).

use std::sync::Arc;

use beam_common::{
    AtomName, BeamError, ExceptionClass, ExitReason, LiveReason, Raise, Result, SourceLocation,
    StackFrame, StackTrace,
};
use beam_loader::Module;
use beam_term::{is_list, is_nil, is_non_value, make_atom, Atom, Eterm, NIL, THE_NON_VALUE};

use crate::context::ExecutionContext;
use crate::error;
use crate::families::exceptions::stacktrace::nearest_line;
use crate::Step;

use super::{Continuation, Machine};

/// One entry of the machine's exception catch-frame stack.
///
/// `try Frame Fail` / `catch Frame Fail` push one of these: the handler's
/// `(func, pc)` target plus the [`frame_index`](CatchFrame::frame_index) of the
/// **call frame that owns the handler** — the frame the machine unwinds back to
/// when a raise lands (C: `next_catch` sets `c_p->stop` to the stack frame that
/// holds the catch tag, `../../erts/emulator/beam/beam_common.c:581-679`). The
/// exception layer ([`crate::families::exceptions`]) consults this stack to turn a
/// raise into a `Step::Goto` to the handler, without touching `crate::interpreter`.
///
/// **Popping rule (the raise never pops).** A raise *reads* the innermost entry
/// and unwinds the call frames, but leaves the entry in place; the **handler
/// opcode** (`try_case`/`try_end`/`catch_end`) pops it. Exactly one handler opcode
/// runs per `try`/`catch` — on the normal path or on the exceptional one — so it
/// consumes exactly one entry. C splits the same bookkeeping in two: `next_catch`
/// clears the stack catch tag while the handler opcode decrements `c_p->catches`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatchFrame {
    /// The function index (into [`Module::functions`](beam_loader::Module::functions))
    /// of the handler.
    pub func: u32,
    /// The code offset of the handler within that function.
    pub pc: u32,
    /// Index into [`Machine::frames`](Machine) of the call frame owning the
    /// handler; a raise unwinds the call-frame stack back to it (and drops its Y
    /// slots' descendants) before landing the exception.
    pub frame_index: usize,
    /// The **module context** the handler's `(func, pc)` belong to (`0` = the run's
    /// entry module, otherwise a [`ModuleContext`](super::ModuleContext) id).
    ///
    /// Stamped by [`Machine::push_catch_frame`] from the context executing when the
    /// handler was pushed, so a raise that unwinds *across* a cross-module
    /// `call_ext` lands back in the handler's own module: the landing address is a
    /// function index into that context's module, and resolving it against the
    /// raising module would be a wrong result.
    pub(crate) context: u32,
}

impl CatchFrame {
    /// A catch frame whose handler is at `pc` in the function at index `func`,
    /// living in the call frame at `frame_index`.
    ///
    /// The module context is **not** a parameter: [`Machine::push_catch_frame`]
    /// stamps it from the context executing when the frame is pushed.
    pub fn new(func: u32, pc: u32, frame_index: usize) -> CatchFrame {
        CatchFrame {
            func,
            pc,
            frame_index,
            context: 0,
        }
    }
}

impl Machine {
    // -----------------------------------------------------------------------
    // Exception scaffolding (the shared state the `families::exceptions` bodies use)
    // -----------------------------------------------------------------------

    /// The number of entries on the exception catch-frame stack.
    ///
    /// Introspection mirroring [`Machine::frame_depth`]/[`Machine::stack_depth`];
    /// the stack is populated by `try`/`catch` (see [`Machine::push_catch_frame`]).
    pub fn catch_frame_depth(&self) -> usize {
        self.catch_frames.len()
    }

    /// The pending exception `(class, reason)`, if a raise has been recorded.
    ///
    /// Set by [`Machine::throw`]; a host can inspect it after a run propagates a
    /// raise. `None` when no raise has been recorded.
    pub fn pending_exception(&self) -> Option<(ExceptionClass, Eterm)> {
        self.pending_exception
    }

    /// The write half of the pending-exception pair, whose read half is
    /// [`Machine::pending_exception`].
    ///
    /// It hands out a mutable reference into the **same** slot the read accessor
    /// returns, so a host that owns a moving (copying) heap can write the
    /// **relocated** reason term back after a collection moves the process heap:
    /// the `Eterm` reason is a GC root the collector must fix up, and
    /// [`Machine::pending_exception`] yields only a copy of it. `None` when no
    /// raise has been recorded.
    ///
    /// The slot is the pending exception `(class, reason)` set by
    /// [`Machine::throw`]/[`Machine::throw_with_trace`].
    pub fn pending_exception_mut(&mut self) -> Option<&mut (ExceptionClass, Eterm)> {
        self.pending_exception.as_mut()
    }
    /// The class of the exception currently being handled, if any.
    ///
    /// This is the engine's stand-in for C's `c_p->freason` exception class
    /// (`../../erts/emulator/beam/error.h:59-87`): set by every [`Machine::throw`].
    /// The `raise` opcode re-raises with **this** class — C's `i_raise` copies the
    /// reason and the raw stacktrace but never touches `freason`
    /// (`../../erts/emulator/beam/emu/instrs.tab:1237-1244`), which is why a
    /// non-matching catch clause re-raises with the original class (verified
    /// against the oracle: `try (try exit(x) catch error:_ -> n end) catch C:R ->
    /// {C,R} end` yields `{exit,x}`).
    pub fn pending_class(&self) -> Option<ExceptionClass> {
        self.pending_exception.map(|(class, _)| class)
    }

    /// Consume the **live raise**'s synthesized-callee-head latch
    /// ([`Machine::raise_head_item`]).
    ///
    /// `true` means the exception currently in flight was raised by a call site
    /// that knew its unresolvable callee and published that callee's
    /// `{Module,Function,ArgList,[]}` item as the raise's raw trace — so the cooked
    /// stacktrace must cons that item in front of its frames (C's innermost
    /// `build_stacktrace` item, `../../erts/emulator/beam/beam_common.c:1204-1292`).
    ///
    /// Taking it consumes it: the flag describes exactly **one** cooking. `false`
    /// for a raise that carried no such item and for a machine with no raise in
    /// flight.
    pub(crate) fn take_raise_head_item(&mut self) -> bool {
        std::mem::take(&mut self.raise_head_item)
    }

    /// Consume the **live raise**'s failed-clause argument list
    /// ([`Machine::raise_function_clause_args`]).
    ///
    /// `Some(args)` means the exception currently in flight was raised by the
    /// `function_clause` site of the function whose clause failed, and `args` is that
    /// function's real call argument list — C's cooked item for the **failing**
    /// function carries the argument list in its `A` slot (`erts_build_mfa_item`,
    /// `../../erts/emulator/beam/utils.c:3430-3457`), while every outer frame keeps
    /// its declared arity. `None` for every other raise.
    ///
    /// Taking it consumes it: the list describes exactly **one** cooking.
    pub(crate) fn take_raise_function_clause_args(&mut self) -> Option<Eterm> {
        self.raise_function_clause_args.take()
    }

    /// The **read half** of [`Machine::take_raise_function_clause_args`]: the live
    /// failed-clause argument list, **without** consuming it.
    ///
    /// The cooked-trace builder needs the list's *length* to size its single rooted
    /// reservation before taking it. It must not copy the list word into a Rust local
    /// for later use, though: a collecting host may **move** the list under that
    /// reservation, so the word returned here is only valid until the next collecting
    /// allocation. The builder therefore consumes the list *after* the reservation,
    /// through [`Machine::take_raise_function_clause_args`], which yields the
    /// re-seated (post-move) word — the machine parks and re-seats the latch for
    /// exactly that reason (see [`Machine::with_roots`]). `None` when no
    /// `function_clause` raise is in flight.
    pub(crate) fn live_raise_function_clause_args(&self) -> Option<Eterm> {
        self.raise_function_clause_args
    }

    /// Consume the **live raise**'s explicitly supplied symbolic frames
    /// ([`Machine::raise_supplied_trace`]).
    ///
    /// `Some(frames)` means the exception currently in flight **arrived carrying
    /// them** (e.g. `erlang:raise/3` with a non-empty trace list), and the
    /// caught-path cooked stacktrace must render THOSE frames verbatim — the
    /// caller-supplied trace replaces the engine's `raise_snapshot` heuristic,
    /// exactly as the oracle's `build_stacktrace` returns the supplied list. `None`
    /// for every raise that carried none, which keeps the `raise_snapshot`-derived
    /// trace in force.
    ///
    /// Taking it consumes it: the frames describe exactly **one** cooking.
    pub(crate) fn take_raise_supplied_trace(&mut self) -> Option<StackTrace> {
        self.raise_supplied_trace.take()
    }

    /// Push a catch frame (the `try`/`catch` handler target).
    ///
    /// The frame's module context is stamped from the context executing right now
    /// ([`Machine::current_context`]), so the handler's `(func, pc)` are always
    /// resolved against the module that pushed them — even when the raise that
    /// lands there unwinds across a cross-module `call_ext`.
    pub fn push_catch_frame(&mut self, mut frame: CatchFrame) {
        frame.context = self.context;
        self.catch_frames.push(frame);
    }

    /// Pop the innermost catch frame, if any.
    pub fn pop_catch_frame(&mut self) -> Option<CatchFrame> {
        self.catch_frames.pop()
    }

    /// Pop the innermost catch frame, or a typed error when the stack is empty.
    ///
    /// Used by the handler opcodes (`try_case`/`try_end`/`catch_end`): they consume
    /// exactly one entry, so an empty stack means the bytecode is unbalanced — a
    /// typed error, never a silent (and wrong) landing.
    pub(crate) fn pop_catch_frame_checked(&mut self, what: &str) -> Result<CatchFrame> {
        self.catch_frames
            .pop()
            .ok_or_else(|| error::unsupported(what))
    }

    /// Install the resolved atom for one exception class.
    ///
    /// The exceptions family calls this with the result of looking the class name
    /// (`error`/`exit`/`throw`) up in the running module's atom table, so that a
    /// landing can place the atom in `x(3)` exactly as C does.
    pub(crate) fn set_class_atom(&mut self, class: ExceptionClass, atom: Eterm) {
        if let Some(index) = exception_class_index(class) {
            self.class_atoms[index] = atom;
        }
    }

    /// Resolve **every still-unresolved** exception-class atom from the engine's
    /// live atom namespace and install it ([`Machine::set_class_atom`]).
    ///
    /// This is the single choke point that keeps a raise from being abandoned when
    /// the *running module's* atom table cannot name a class atom (`error`/`exit`/
    /// `throw`): C reads its `am_error`/`am_exit`/`am_throw` **globals**
    /// (`../../erts/emulator/beam/beam_common.c:541-544`), so every class is
    /// nameable there, while a module's load-time snapshot only holds the names its
    /// own source (plus the loader's seeds) interned. The namespace
    /// ([`ExecutionContext::atoms`]) is the engine's equivalent of those globals —
    /// the runtime answers with its ONE live shared table — and
    /// [`namespace_atom`] falls back to the module snapshot, so a host that does not
    /// override the seam behaves exactly as before.
    ///
    /// **Cost and ownership.** The whole body is skipped by one `bool` test once it
    /// has run (`class_atoms_live`, cleared by [`Machine::reset`] — the class atoms
    /// belong to the run that is starting), so the only caller
    /// ([`crate::families::dispatch`], one call per family-owned instruction) pays a
    /// single load in steady state. An atom **neither** source can name stays
    /// unresolved on purpose: [`Machine::class_atom`] then reports its typed
    /// `unsupported` rather than substituting a wrong atom.
    pub(crate) fn ensure_class_atoms(&mut self, ctx: &mut dyn ExecutionContext, module: &Module) {
        if self.class_atoms_live {
            return;
        }
        for class in [
            ExceptionClass::Error,
            ExceptionClass::Exit,
            ExceptionClass::Throw,
        ] {
            let Some(index) = exception_class_index(class) else {
                continue;
            };
            if !is_non_value(self.class_atoms[index]) {
                // Already installed by a raise site (from the same namespace, so the
                // same index) — never overwrite it.
                continue;
            }
            if let Some(atom) = namespace_atom(ctx, module, class.as_str()) {
                self.class_atoms[index] = make_atom(atom);
            }
        }
        self.class_atoms_live = true;
    }
    /// The installed atom for `class`, or a typed error when unset/unknown.
    ///
    /// A missing atom means **neither** the engine's live atom namespace nor the
    /// running module's atom table names the class (see
    /// [`Machine::ensure_class_atoms`], which resolves all three classes from the
    /// namespace before the raise lands). The engine (correctly) refuses to
    /// substitute a different atom: the class term would then be a *different,
    /// wrong* atom rather than the documented `unsupported` error.
    fn class_atom(&self, class: ExceptionClass) -> Result<Eterm> {
        let atom = exception_class_index(class)
            .map(|index| self.class_atoms[index])
            .filter(|atom| !is_non_value(*atom));
        atom.ok_or_else(|| {
            error::unsupported(&format!(
                "exceptions: no atom for the '{}' exception class in the module atom table",
                class.as_str()
            ))
        })
    }
    /// Unwind the call-frame stack back to `frame_index` (and drop the Y slots of
    /// every deeper frame), reproducing C's `c_p->stop = prev` in `next_catch`
    /// (`../../erts/emulator/beam/beam_common.c:643-679`).
    pub(crate) fn unwind_to_frame(&mut self, frame_index: usize) -> Result<()> {
        let frame = *self
            .frames
            .get(frame_index)
            .ok_or_else(|| error::unsupported("exceptions: stale catch frame"))?;
        self.frames.truncate(frame_index + 1);
        let keep = frame.y_base + frame.y_count as usize;
        if keep > self.stack.len() {
            return Err(error::unsupported(
                "exceptions: catch frame stack underflow",
            ));
        }
        self.stack.truncate(keep);
        Ok(())
    }

    /// Raise a BEAM exception with `reason` of class `class` and an empty
    /// (Milestone 1 captures no stacktrace) raw stacktrace.
    ///
    /// **This is the one uniform raise/unwind entry point** every instruction
    /// family calls (the wave-2 contract); the exact semantics are documented on
    /// [`Machine::throw_with_trace`], which this forwards to with `trace = NIL`.
    pub(crate) fn throw(&mut self, class: ExceptionClass, reason: Eterm) -> Result<Step> {
        self.throw_with_trace(class, reason, NIL)
    }

    /// [`Machine::throw`] with class [`ExceptionClass::Error`] — the convenience
    /// for the `{badmatch,V}`/`{case_clause,V}`/`{badrecord,V}`-style raises the
    /// exception family emits.
    pub(crate) fn throw_error(&mut self, reason: Eterm) -> Result<Step> {
        self.throw(ExceptionClass::Error, reason)
    }

    /// Raise a BEAM exception with class `class`, `reason` and raw stacktrace
    /// `trace` (C: `c_p->fvalue`/`c_p->ftrace` at `find_func_info`).
    ///
    /// **The one uniform raise/unwind entry point** every instruction family calls
    /// (the wave-2 contract). A raise is *not* a bare `BeamError` — it is a
    /// control-flow decision:
    ///
    /// 1. The pending exception `(class, reason)` is recorded on the machine
    ///    ([`Machine::pending_exception`]); it is also the class a later `raise`
    ///    opcode re-raises with ([`Machine::pending_class`]).
    /// 2. If a catch frame is live ([`Machine::catch_frame_depth`] `> 0`) the
    ///    **innermost** one is consulted: the call-frame stack is unwound to
    ///    [`CatchFrame::frame_index`], the exception is landed in the handler
    ///    registers ([`Machine::land_exception`]) and the result is
    ///    `Ok(Step::Goto { func, pc })` for that handler. The catch frame itself is
    ///    **not** popped — the handler opcode that runs next pops it (see
    ///    [`CatchFrame`]).
    /// 3. If no catch frame is live the result is the typed
    ///    `Err(BeamError::Raise(..))` the run propagates — **never a panic**. The
    ///    live reason *term* rides out of the machine in that raise's
    ///    [`Raise::live_reason`] slot ([`LiveReason::new`], payload [`Eterm`]) while
    ///    [`ExitReason::Structured`] keeps [`BeamError::as_atom`] answering `None`,
    ///    so a caller with a term representation can recover the compound reason.
    ///
    /// The symbolic [`StackTrace`] an **uncaught** raise carries is deliberately
    /// **not** a parameter here: it is attached uniformly by
    /// [`Machine::with_uncaught_frames`] as the raise leaves the machine (at the
    /// [`Machine::step_instruction`](crate::machine::Machine) /
    /// [`Machine::drive`](crate::machine::Machine) boundary), so **every** raise kind
    /// — a `throw`/`error`/`exit` family builder, a BIF failure, the `func_info`
    /// failed-clause raise — gets the same localizable frames (what makes a bare-atom
    /// `function_clause`/`badmatch` reason name its function) without each site
    /// deriving them. A **caught** raise never carries them out: the frames are
    /// metadata on the uncaught `Raise` only, so the landing registers
    /// ([`Machine::land_exception`]), the reason and the pending class are untouched
    /// and a caught raise behaves byte-identically.
    ///
    /// A family body typically ends with `machine.throw(class, reason).map(Some)`
    /// (or, for a `{badmatch,V}`-style raise, [`Machine::throw_error`]).
    pub(crate) fn throw_with_trace(
        &mut self,
        class: ExceptionClass,
        reason: Eterm,
        trace: Eterm,
    ) -> Result<Step> {
        // Consume the one-shot synthesized-callee-head latch **first**, before
        // anything else, so no raise can ever inherit a stale head: the head is
        // carried by `trace` itself (the raise's raw-trace word), and this flag only
        // records that the cooked trace must prepend it. A raise raised by a site
        // that published no head clears the state to `false`.
        self.raise_head_item = std::mem::take(&mut self.pending_callee_head);
        // The failed-clause argument list rides the same two-latch protocol: the raise
        // site armed the one-shot latch, and this raise — whoever raised it — owns the
        // live state from here on, so a raise that carries no such list clears it to
        // `None` (never a stale list from an earlier raise).
        self.raise_function_clause_args = self.pending_function_clause_args.take();
        // The explicitly **supplied** symbolic frames ride the same protocol: the
        // raise site armed the one-shot latch (only when the incoming raise carried
        // a non-empty trace), and this raise — whoever raised it — owns the live
        // state from here on, so a raise that arrived with no such frames clears it.
        self.raise_supplied_trace = self.pending_supplied_trace.take();
        self.pending_exception = Some((class, reason));
        // Rebuild the **pre-unwind** raise snapshot before anything is unwound: the
        // handler's `build_stacktrace` (and the uncaught funnel) must be able to name
        // the frames the raise unwinds out of. `step_instruction` records
        // `current_code` at its head, so it is exactly the raising instruction; the
        // live callers come from the call-frame stack, innermost first. This is the
        // engine's replacement for C's saved raw `StackTrace` (`c_p->ftrace`).
        self.raise_snapshot.clear();
        if let Some((func, pc)) = self.current_code() {
            self.raise_snapshot.push((self.current_context(), func, pc));
        }
        for (func, pc, context) in self.resumption_contexts() {
            self.raise_snapshot.push((context, func, pc));
        }
        let frame = match self.catch_frames.last() {
            Some(frame) => *frame,
            // An uncaught structured raise carries its live reason term out of the
            // machine in the raise's `live_reason` slot; `ExitReason::Structured`
            // keeps `as_atom()` at `None`. The symbolic frames are attached later,
            // by `Machine::with_uncaught_frames`.
            None => {
                return Err(BeamError::Raise(
                    Raise::new(class, ExitReason::Structured)
                        .with_live_reason(LiveReason::new(reason)),
                ))
            }
        };
        // The class atom is only needed when the raise actually lands; a missing
        // atom (absent from the module's atom table) is a typed error, and an
        // *uncaught* raise above never reaches this point.
        let class_atom = self.class_atom(class)?;
        self.unwind_to_frame(frame.frame_index)?;
        self.land_exception(class_atom, reason, trace)?;
        // The handler's `(func, pc)` are indices into the *landing context's*
        // module, which is not necessarily the module the raise happened in (a raise
        // can unwind across a cross-module `call_ext`). Report the context change so
        // the driver re-binds the module before executing the handler; an
        // intra-module landing stays the plain `Goto` it always was.
        if frame.context == self.context {
            Ok(Step::Goto {
                func: frame.func,
                pc: frame.pc,
            })
        } else {
            Ok(Step::Enter {
                context: frame.context,
                func: frame.func,
                pc: frame.pc,
            })
        }
    }

    /// Attach the symbolic frames of an **uncaught** raise to `err`, when it is a
    /// [`BeamError::Raise`] that carries none yet.
    ///
    /// This is the single funnel that makes **every** raise kind localizable: a
    /// raise leaves the machine as a bare `BeamError::Raise` with an empty
    /// [`StackTrace`] (only the raising *address* is known at the raise site), and
    /// [`Machine::raise_stacktrace`] derives the frames from the live call frames
    /// plus that address. Running this at the one boundary the interpreter and the
    /// native tiers both pass through (see
    /// [`Machine::step_instruction`](crate::machine::Machine) and
    /// [`Machine::drive`](crate::machine::Machine)) means no individual raise site —
    /// the `throw`/`error`/`exit` family builders, a BIF failure through
    /// [`Machine::raise_bif_error`], the `func_info` failed-clause raise — has to
    /// derive them, and all agree on the same frames.
    ///
    /// * `entry_module` is the run's entry module (module context `0`). `None` means
    ///   the caller cannot name that module at this point, so any context-`0` frame
    ///   is **skipped** — a frame is never named against the wrong module.
    /// * `func`/`pc` are the raising instruction's address.
    ///
    /// A raise that already carries frames (a caller that has run this before),
    /// a *caught* raise (which never reaches here — it is an `Ok(Step)` landing),
    /// or any non-[`BeamError::Raise`] error is returned **unchanged**. When the
    /// derived trace is empty the raise is returned unchanged too, so a raise the
    /// module cannot name is never given an invented frame.
    ///
    /// **Engine diagnostics are deliberately excluded** ([`is_engine_diagnostic`]):
    /// a [`BeamError::Raise`] whose reason is the [`crate::error::unsupported`]
    /// sentinel describes a **coverage limit of the engine itself** (an
    /// unimplemented opcode/operand, a stale catch frame, `deallocate: frame size
    /// mismatch`, a `receive` that cannot suspend), not an exception the Erlang
    /// program raised. Such a diagnostic is not localizable *as a program position*
    /// and — crucially — reaches the driver through **two different paths**: the
    /// interpreter's every-diagnostic goes through [`Machine::step_instruction`],
    /// while the native baseline tier's linked-tail-call helper
    /// (`jit_tail_call`, which calls [`Machine::pop_frame_checked`] directly) reports
    /// its own diagnostic to the driver without passing through this funnel. Frames
    /// here would therefore make the two tiers disagree on the *same* error's
    /// [`Debug`](std::fmt::Debug) — which the
    /// `interpreter_and_baseline_agree_over_the_real_error_path_corpus` gate forbids.
    /// So `error::unsupported` is the single choke point for engine diagnostics and
    /// they stay frame-less; every **program-level** raise (class `throw`/`error`/
    /// `exit`, a BIF's own `badarg`/`badmatch`/… reason, the `func_info`
    /// `function_clause`) is decorated.
    pub(crate) fn with_uncaught_frames(
        &mut self,
        err: BeamError,
        entry_module: Option<&Module>,
        func: u32,
        pc: u32,
    ) -> BeamError {
        match err {
            BeamError::Raise(raise)
                if raise.stacktrace.is_empty() && !is_engine_diagnostic(&raise.reason) =>
            {
                // Consume the failed-clause argument list: it belongs to the raising
                // frame, which is the frame this funnel names first.
                let failed_clause_args = self.take_raise_function_clause_args();
                let frames = self.raise_stacktrace(entry_module, func, pc, failed_clause_args);
                if frames.is_empty() {
                    BeamError::Raise(raise)
                } else {
                    BeamError::Raise(raise.with_stacktrace(frames))
                }
            }
            other => other,
        }
    }

    /// Land a *caught* exception in the handler registers.
    ///
    /// Reproduces the C convention established before `next_catch` jumps to the
    /// handler (`../../erts/emulator/beam/beam_common.c:535-544`, `handle_error`):
    ///
    /// * `x(0) = THE_NON_VALUE` — the "the previous code completed abnormally"
    ///   marker that `catch_end`/`try_case` test (`emu/instrs.tab:1175-1227`);
    /// * `x(1) = reason` (the *expanded* error term, `expand_error_value`);
    /// * `x(2) = trace` — the raw stacktrace (Milestone 1 captures none, so `NIL`);
    /// * `x(3) = class_atom` — `exception_tag[GET_EXC_CLASS(freason)]`, installed by
    ///   [`Machine::set_class_atom`]; the handler only has to move it to `x(0)`
    ///   (`try_case`) or compare it against `throw`/`error` (`catch_end`).
    fn land_exception(&mut self, class_atom: Eterm, reason: Eterm, trace: Eterm) -> Result<()> {
        self.set_x(0, Eterm::non_value())?;
        self.set_x(1, reason)?;
        self.set_x(2, trace)?;
        self.set_x(3, class_atom)?;
        Ok(())
    }

    /// The [`StackTrace`] of a raise happening at `(func, pc)` **inside
    /// `entry_module`**: the raising function first, then every live caller frame,
    /// innermost first.
    ///
    /// C captures this chain as the raw `StackTrace` object hung off the exception
    /// (`c_p->ftrace`, the saved continuation pointers `build_stacktrace` later
    /// resolves); the engine has no saved object, so it derives the frames from the
    /// state it does hold — the live call frames, whose continuation-pointer slots
    /// are `(func, pc)` pairs ([`Machine::resumption_addresses`]) — plus the
    /// `(func, pc)` the dispatch loop is executing right now, which its caller
    /// supplies because the machine does not track it inside an instruction. The
    /// order is C's (`build_stacktrace` prepends the current function last,
    /// `../../erts/emulator/beam/beam_common.c:1274-1288`).
    ///
    /// **Which module a frame belongs to.** A code address identifies its own
    /// module (C's model, and this engine's: a frame's [`Continuation`] carries the
    /// module context it returns into). `entry_module` is the run's entry module
    /// (context `0`, which the machine borrows rather than owns) — `None` means the
    /// caller cannot name it at this point, so any context-`0` frame is **skipped**
    /// rather than attributed to a module it does not belong to; a registered
    /// callee's module comes from [`Machine::module_context`]. A frame whose context
    /// names neither (context `0` with no entry module, or an id that was never
    /// registered) is **skipped** rather than resolved against the wrong module — a
    /// frame is never named with a module the address does not belong to.
    ///
    /// A frame whose function index is out of range, or whose module/function atom
    /// the module's own table cannot name, is likewise skipped: this is
    /// best-effort *metadata* attached to an uncaught raise, and it must never turn
    /// a raise into a different error (or invent a name).
    ///
    /// Only the frames are derived: the reason and the class are untouched.
    pub(crate) fn raise_stacktrace(
        &self,
        entry_module: Option<&Module>,
        func: u32,
        pc: u32,
        first_frame_args: Option<Eterm>,
    ) -> StackTrace {
        let mut trace = StackTrace::new();
        // The raising function itself — the `(func, pc)` of the instruction that
        // raised, in the module context executing right now. `first_frame_args` is the
        // failed clause's argument list, when this raise came from the
        // `function_clause` site (see [`Machine::raise_function_clause_args`]); it
        // belongs to THIS frame and to no other.
        if let Some(module) = self.frame_module(entry_module, self.context) {
            push_stack_frame(&mut trace, &module, func, pc, first_frame_args);
        }
        // Then the live caller chain, innermost frame first. The bottom frame's
        // `Continuation::Halt` contributes nothing (there is no address to name).
        for frame in self.frames.iter().rev() {
            let Continuation::Address { func, pc, context } = frame.continuation else {
                continue;
            };
            if let Some(module) = self.frame_module(entry_module, context) {
                push_stack_frame(&mut trace, &module, func, pc, None);
            }
        }
        trace
    }

    /// The module a frame in module context `id` belongs to — the borrowed entry
    /// module for context `0` (when the caller can name it), else the registered
    /// context's `Arc<Module>`.
    ///
    /// `None` for context `0` when no entry module is known, and for a context that
    /// was never registered (see [`Machine::raise_stacktrace`]).
    fn frame_module<'a>(
        &self,
        entry_module: Option<&'a Module>,
        id: u32,
    ) -> Option<FrameModule<'a>> {
        if id == 0 {
            return entry_module.map(FrameModule::Entry);
        }
        self.module_context(id)
            .map(|handle| FrameModule::Registered(Arc::clone(&handle.module)))
    }
}

/// The module a stack frame belongs to: the run's borrowed entry module (context
/// `0`) or an owned handle to a registered callee context's module.
enum FrameModule<'a> {
    /// The run's entry module, owned by the caller of the run (never by the
    /// machine), so it is borrowed.
    Entry(&'a Module),
    /// A registered module context's module ([`Machine::module_context`]).
    Registered(Arc<Module>),
}

impl std::ops::Deref for FrameModule<'_> {
    type Target = Module;

    fn deref(&self) -> &Module {
        match self {
            FrameModule::Entry(module) => module,
            FrameModule::Registered(module) => module,
        }
    }
}

/// Append one [`StackFrame`] naming `(func, pc)` of `module`.
///
/// An out-of-range `func`, or a module/function atom the module's own table cannot
/// name, appends nothing: the frames are best-effort metadata and must never
/// invent a name (see [`Machine::raise_stacktrace`]).
///
/// `failed_clause_args` is the failed clause's real argument list for the **one**
/// frame whose function contains the failed clause (C's `args`,
/// `erts_build_mfa_item`); it is `None` for every other frame. A
/// [`StackFrame`](beam_common::StackFrame) carries only an **arity**, so the list
/// cannot be represented here — and it does not have to be: the list is built from
/// exactly the function's `arity` argument registers, so its length IS that arity.
/// C's rule (`erts_build_mfa_item`: the argument list when it is a list/nil, else
/// the declared arity) therefore renders identically either way, and the parameter
/// exists to state that rule at the one frame it applies to.
fn push_stack_frame(
    trace: &mut StackTrace,
    module: &Module,
    func: u32,
    pc: u32,
    failed_clause_args: Option<Eterm>,
) {
    let Some(function) = module.functions.get(func as usize) else {
        return;
    };
    let (Some(module_name), Some(function_name)) = (
        module.atom_name(module.name),
        module.atom_name(function.name),
    ) else {
        return;
    };
    let arity = match failed_clause_args {
        // The argument list, when it is the list/nil C emits in the `A` slot; its
        // length is the failed function's arity by construction.
        Some(args) if is_list(args) || is_nil(args) => {
            failed_clause_arity(args).unwrap_or(function.arity as u8)
        }
        _ => function.arity as u8,
    };
    let mut frame = StackFrame::new(module_name.to_string(), function_name.to_string(), arity);
    frame.location = source_location(module, func, pc);
    trace.push(frame);
}

/// The number of elements of the proper list `list`, or `None` when it is not a
/// proper list (an improper tail).
///
/// Only used for the **failed clause's** argument list, which
/// [`crate::interpreter::raise_function_clause_with_args`] builds as a proper list of
/// the call's argument registers — its length is the failed function's arity, which
/// is what C's `erts_build_mfa_item` emits for a list/nil `args`.
fn failed_clause_arity(list: Eterm) -> Option<u8> {
    let mut count = 0u8;
    let mut cursor = list;
    while !is_nil(cursor) {
        cursor = crate::terms::cons_tail(cursor).ok()?;
        count = count.checked_add(1)?;
    }
    Some(count)
}

/// The [`SourceLocation`] in effect at code offset `pc` of the function at index
/// `func`, when the module knows one.
///
/// The `file` name comes from the module's `Line` chunk and the line from the
/// `line` marker in effect — the same `nearest_line` lookup the symbolic
/// (`build_stacktrace`) trace uses, so the two agree.
fn source_location(module: &Module, func: u32, pc: u32) -> Option<SourceLocation> {
    let entry = nearest_line(module, func, pc)?;
    let file = module.line.files.get(entry.file as usize)?;
    Some(SourceLocation {
        file: AtomName::new(file.to_string()),
        line: entry.line,
    })
}

/// The class atoms a landing must be able to name, in [`Machine`]'s
/// `class_atoms` slot order.
fn exception_class_index(class: ExceptionClass) -> Option<usize> {
    match class {
        ExceptionClass::Error => Some(0),
        ExceptionClass::Exit => Some(1),
        ExceptionClass::Throw => Some(2),
        _ => None,
    }
}

/// Resolve an atom **name** through the engine's live atom namespace
/// ([`ExecutionContext::atoms`]) with the running module's load-time snapshot as
/// the fallback.
///
/// This is the one definition of "name an atom for a raise" the raise path uses.
/// The default [`ExecutionContext::atoms`] body **is** `&module.atom_table`, so a
/// host that does not override the seam gets exactly the pre-seam behaviour; a host
/// that owns ONE live shared namespace (the runtime) is asked first, because a
/// module's snapshot cannot name an atom interned *after* it was loaded (its id is
/// beyond the snapshot) — and an exception's reason/class atoms are exactly such
/// runtime-interned names.
///
/// A name **neither** source knows yields `None`, and every caller keeps its typed
/// error / no-substitution path: the engine never invents an atom index, because an
/// index is valid only against the table it was read with.
///
/// Borrow discipline: `atoms` borrows `*ctx` immutably for the length of the
/// lookup only, and the result is a `Copy` [`Atom`], so the borrow is over before
/// the caller's next `&mut ctx` call.
pub(crate) fn namespace_atom(
    ctx: &dyn ExecutionContext,
    module: &Module,
    name: &str,
) -> Option<Atom> {
    ctx.atoms(module)
        .lookup(name)
        .or_else(|| module.atom_table.lookup(name))
}
/// The reason-atom prefix of every **engine diagnostic** raise
/// ([`crate::error::unsupported`]).
const ENGINE_DIAGNOSTIC_PREFIX: &str = "unsupported_opcode: ";

/// Whether `reason` describes an **engine diagnostic** rather than a
/// program-level exception.
///
/// A [`crate::error::unsupported`] raise means "this engine cannot execute this
/// opcode/operand", "this state is inconsistent", "this run cannot suspend" —
/// a *coverage limit of the runtime*, not an exception the Erlang program raised.
/// Such a reason is always an atom beginning [`ENGINE_DIAGNOSTIC_PREFIX`]; no
/// program-level reason (`badarg`, `function_clause`, `badmatch`, a `throw`/`exit`
/// reason, …) ever carries that prefix, so the test is exact rather than
/// heuristic.
///
/// The distinction matters for the uniform raise funnel
/// ([`Machine::with_uncaught_frames`]): diagnostics reach the driver through two
/// different paths (the interpreter funnels every instruction's diagnostic through
/// [`Machine::step_instruction`]; the native baseline tier's linked-tail-call helper
/// reports [`Machine::pop_frame_checked`]'s own diagnostic straight to the driver),
/// so decorating them would make the tiers disagree on the same error's
/// [`Debug`](std::fmt::Debug). See [`Machine::with_uncaught_frames`].
fn is_engine_diagnostic(reason: &ExitReason) -> bool {
    reason
        .as_atom()
        .is_some_and(|name| name.starts_with(ENGINE_DIAGNOSTIC_PREFIX))
}

/// The "no class atom resolved yet" marker (`THE_NON_VALUE` is not a term any
/// atom could be).
///
/// It is the machine's initial `class_atoms` value (set by
/// [`Machine::with_max_register`]) and the value [`Machine::reset`] restores.
/// Shared through `pub(super)`, so it stays private to the `machine` module tree.
pub(super) const UNRESOLVED_CLASS_ATOMS: [Eterm; 3] = [THE_NON_VALUE; 3];

#[cfg(test)]
mod tests {
    use super::{CatchFrame, Machine};
    use beam_common::{BeamError, ExceptionClass, StackFrame, StackTrace};
    use beam_loader::{Atom, Function, Instr, Label, LineEntry, Module};
    use beam_term::{make_atom, Eterm, NIL};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use crate::error;
    use crate::machine::Continuation;
    use crate::Step;

    /// A module named `name` with one bare `return` function per `(name, arity)`
    /// pair; the code itself is irrelevant to a stack frame.
    fn module_with(name: &str, functions: &[(&str, u32)]) -> Module {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        for (index, (function_name, arity)) in functions.iter().enumerate() {
            let function_atom = module.atom_table.intern(function_name);
            let module_atom = module.name;
            module.functions.push(Function {
                func_label: Label(index as u32),
                entry_label: Label(index as u32),
                module: module_atom,
                name: function_atom,
                arity: *arity,
                code: vec![Instr::Return],
                blocks: Vec::new(),
                labels: BTreeMap::new(),
            });
        }
        module
    }
    #[test]
    fn throw_without_a_catch_frame_is_a_typed_error() {
        let mut machine = Machine::new();
        let reason = Eterm::from_raw(0xBEEF);
        let outcome = machine.throw(ExceptionClass::Throw, reason);
        // No catch frame: the raise propagates as a typed error, never a panic.
        assert!(matches!(outcome, Err(BeamError::Raise(_))));
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Throw, reason))
        );
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    #[test]
    fn throw_to_a_catch_frame_lands_the_exception_and_leaves_the_entry_for_the_handler() {
        let mut machine = Machine::new();
        // Frame 0 (the bottom frame) with no Y slots: the frame the handlers below
        // pretend to live in.
        machine.push_frame(0).unwrap();
        // Two live frames: only the innermost must be consulted.
        machine.push_catch_frame(CatchFrame::new(3, 7, 0));
        machine.push_catch_frame(CatchFrame::new(9, 9, 0));
        let class_atom = make_atom(Atom::from_raw(11));
        machine.set_class_atom(ExceptionClass::Error, class_atom);
        let reason = Eterm::from_raw(0x1234);

        let outcome = machine.throw_error(reason).expect("a live frame catches");
        assert_eq!(outcome, Step::Goto { func: 9, pc: 9 });
        // The raise consults the innermost entry but does **not** pop it: the
        // handler opcode that runs next consumes exactly one entry.
        assert_eq!(machine.catch_frame_depth(), 2);
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Error, reason))
        );
        assert_eq!(machine.pending_class(), Some(ExceptionClass::Error));

        // The C handler convention (beam_common.c:535-544): x0 = THE_NON_VALUE,
        // x1 = reason, x2 = [] stacktrace, x3 = the class atom.
        assert_eq!(machine.x(0), Some(Eterm::non_value()));
        assert_eq!(machine.x(1), Some(reason));
        assert_eq!(machine.x(2), Some(NIL));
        assert_eq!(machine.x(3), Some(class_atom));
    }

    #[test]
    fn throw_returns_the_outer_frame_on_a_second_raise() {
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(3, 7, 0));
        machine.set_class_atom(ExceptionClass::Exit, make_atom(Atom::from_raw(1)));
        let _ = machine
            .throw(ExceptionClass::Exit, Eterm::from_raw(1))
            .unwrap();
        // The raise leaves the entry in place; the handler opcode consumes it.
        assert_eq!(machine.pop_catch_frame().map(|frame| frame.func), Some(3));
        // The single frame is gone: the next raise cannot be caught.
        assert!(machine
            .throw(ExceptionClass::Exit, Eterm::from_raw(2))
            .is_err());
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    #[test]
    fn throw_without_a_resolved_class_atom_is_a_typed_error() {
        // A landing needs the class atom for `x(3)`; an unresolved atom must never
        // be replaced by a different (wrong) atom.
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(3, 7, 0));
        let err = machine.throw_error(Eterm::from_raw(1)).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
    }

    #[test]
    fn unwind_to_frame_drops_deeper_frames_and_their_y_slots() {
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap(); // frame 0: 0 Y slots
        machine.push_frame(2).unwrap(); // frame 1: y(0), y(1)
        machine.set_y(0, Eterm::from_raw(0xAA)).unwrap();
        machine.push_frame(1).unwrap(); // frame 2: y(0)
        assert_eq!(machine.stack_depth(), 3);

        machine.unwind_to_frame(1).unwrap();
        assert_eq!(machine.frame_depth(), 2);
        assert_eq!(machine.stack_depth(), 2);
        assert_eq!(machine.y(0), Some(Eterm::from_raw(0xAA)));
        // A stale index (no such frame) is a typed error, never a panic.
        assert!(machine.unwind_to_frame(9).is_err());
    }

    // -----------------------------------------------------------------------
    // the raise's stacktrace frames (`Machine::raise_stacktrace`)
    // -----------------------------------------------------------------------

    #[test]
    fn raise_stacktrace_names_the_raising_function_and_the_live_callers() {
        let entry = module_with("m", &[("f", 1), ("g", 2)]);
        let mut machine = Machine::new();
        // The bottom frame, then a frame whose continuation returns into `f/1` at
        // code offset 3 — i.e. `f` is suspended in the call the raising function is
        // executing.
        machine.push_frame(0).unwrap();
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(Continuation::Address {
                func: 0,
                pc: 3,
                context: 0,
            })
            .unwrap();

        let trace = machine.raise_stacktrace(Some(&entry), 1, 7, None);
        assert_eq!(trace.len(), 2, "the raising function then its caller");
        // The function that raised comes first: `g/2` at `(1, 7)`.
        assert_eq!(trace.frames[0].module.as_str(), "m");
        assert_eq!(trace.frames[0].function.as_str(), "g");
        assert_eq!(trace.frames[0].arity, 2);
        assert_eq!(trace.frames[0].location, None, "no line marker in effect");
        // Then the live caller, innermost first.
        assert_eq!(trace.frames[1].module.as_str(), "m");
        assert_eq!(trace.frames[1].function.as_str(), "f");
        assert_eq!(trace.frames[1].arity, 1);
    }

    #[test]
    fn raise_stacktrace_carries_the_location_marker_in_effect() {
        let mut module = module_with("m", &[("f", 1)]);
        module.line = beam_loader::LineTable {
            flags: 0,
            instruction_count: 0,
            items: vec![LineEntry::default(), LineEntry { file: 0, line: 7 }],
            files: vec!["m.erl".to_string()],
        };
        // `label 1; line 1; return` — the marker is in effect at code offset 2.
        module.functions[0].code = vec![
            Instr::Label(Label(1)),
            Instr::Line(1),
            Instr::Return,
        ];

        let trace = Machine::new().raise_stacktrace(Some(&module), 0, 2, None);
        let location = trace.frames[0]
            .location
            .as_ref()
            .expect("the line marker is in effect");
        assert_eq!(location.file.as_str(), "m.erl");
        assert_eq!(location.line, 7);
        // Before the marker there is no location (C's LINE_INVALID_LOCATION).
        assert_eq!(
            Machine::new().raise_stacktrace(Some(&module), 0, 0, None).frames[0].location,
            None
        );
    }

    #[test]
    fn raise_stacktrace_resolves_each_frame_against_its_own_module_context() {
        let entry = module_with("m", &[("f", 1)]);
        let callee = Arc::new(module_with("c", &[("g", 0)]));
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_frame(0).unwrap();
        // The caller frame returns into the ENTRY module (context 0)…
        machine
            .set_continuation(Continuation::Address {
                func: 0,
                pc: 1,
                context: 0,
            })
            .unwrap();
        // …while the machine is executing in a registered callee context, so the
        // raising function belongs to the callee's module.
        let id = machine.register_context(&callee);
        machine.switch_context(id).unwrap();

        let trace = machine.raise_stacktrace(Some(&entry), 0, 5, None);
        assert_eq!(trace.len(), 2);
        assert_eq!(trace.frames[0].module.as_str(), "c", "the raising function");
        assert_eq!(trace.frames[0].function.as_str(), "g");
        assert_eq!(trace.frames[1].module.as_str(), "m", "the cross-module caller");
        assert_eq!(trace.frames[1].function.as_str(), "f");
    }

    #[test]
    fn raise_stacktrace_skips_a_frame_the_module_or_context_cannot_name() {
        let entry = module_with("m", &[("f", 1)]);
        // A function index outside the module's table is skipped, never invented.
        assert!(Machine::new().raise_stacktrace(Some(&entry), 9, 0, None).is_empty());

        // A continuation in a context that was never registered is skipped too: the
        // address cannot be attributed to a module, so it is not named at all.
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(Continuation::Address {
                func: 0,
                pc: 1,
                context: 9,
            })
            .unwrap();
        let trace = machine.raise_stacktrace(Some(&entry), 0, 0, None);
        assert_eq!(trace.len(), 1, "only the raising function is named");
        assert_eq!(trace.frames[0].function.as_str(), "f");
    }

    /// `raise_stacktrace(None, ..)` cannot name the run's entry module, so every
    /// context-`0` frame is **skipped** — while a frame in a *registered* context is
    /// still named (its module comes from the registry, not from the entry module).
    #[test]
    fn raise_stacktrace_without_an_entry_module_skips_context_zero_frames() {
        let callee = Arc::new(module_with("c", &[("g", 0)]));
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_frame(0).unwrap();
        // The caller frame is a context-`0` address (the entry module)…
        machine
            .set_continuation(Continuation::Address {
                func: 0,
                pc: 1,
                context: 0,
            })
            .unwrap();
        // …while the machine executes in a registered callee context.
        let id = machine.register_context(&callee);
        machine.switch_context(id).unwrap();

        // With an entry module: the raising function (callee) and the caller.
        assert_eq!(machine.raise_stacktrace(Some(&module_with("m", &[("f", 1)])), 0, 5, None).len(), 2);
        // Without one: the context-`0` caller frame is skipped, so only the callee's
        // frame remains — never named against the wrong module.
        let trace = machine.raise_stacktrace(None, 0, 5, None);
        assert_eq!(trace.len(), 1, "the context-0 caller is skipped");
        assert_eq!(trace.frames[0].module.as_str(), "c");
        assert_eq!(trace.frames[0].function.as_str(), "g");
    }

    /// A raise starts life with no frames (`throw_with_trace`); the funnel attaches
    /// them. A raise that already carries frames, and a non-`Raise` error, are
    /// returned untouched.
    #[test]
    fn with_uncaught_frames_attaches_only_to_an_empty_raise() {
        let entry = module_with("m", &[("f", 1), ("g", 2)]);
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(Continuation::Address {
                func: 0,
                pc: 3,
                context: 0,
            })
            .unwrap();

        // A fresh, frame-less uncaught raise: the funnel names `g/2` then its caller.
        let bare = machine
            .throw(ExceptionClass::Throw, Eterm::from_raw(0x99))
            .expect_err("no catch frame");
        assert!(matches!(&bare, BeamError::Raise(r) if r.stacktrace.is_empty()));
        match machine.with_uncaught_frames(bare, Some(&entry), 1, 7) {
            BeamError::Raise(raise) => {
                assert_eq!(raise.stacktrace.len(), 2);
                assert_eq!(raise.stacktrace.frames[0].function.as_str(), "g");
                assert_eq!(raise.stacktrace.frames[0].arity, 2);
                assert_eq!(raise.stacktrace.frames[1].function.as_str(), "f");
            }
            other => panic!("expected a raise, got {other:?}"),
        }

        // A raise that already carries frames is left unchanged (the second caller of
        // the funnel does not re-derive them).
        let mut with_frames = StackTrace::new();
        with_frames.push(StackFrame::new("m", "already", 0));
        let carried = BeamError::Raise(
            beam_common::Raise::new(ExceptionClass::Error, beam_common::ExitReason::Structured)
                .with_stacktrace(with_frames),
        );
        match machine.with_uncaught_frames(carried, Some(&entry), 1, 7) {
            BeamError::Raise(raise) => {
                assert_eq!(raise.stacktrace.len(), 1);
                assert_eq!(raise.stacktrace.frames[0].function.as_str(), "already");
            }
            other => panic!("expected a raise, got {other:?}"),
        }

        // A non-raise error is untouched.
        assert!(matches!(
            machine.with_uncaught_frames(BeamError::BadArg, Some(&entry), 1, 7),
            BeamError::BadArg
        ));

        // A raise the module cannot name keeps an empty trace rather than inventing a
        // frame: the raising function index is out of range AND the machine holds no
        // live caller frame to name either.
        let mut frameless = Machine::new();
        let bare = frameless
            .throw(ExceptionClass::Throw, Eterm::from_raw(0x11))
            .expect_err("no catch frame");
        match frameless.with_uncaught_frames(bare, Some(&entry), 9, 0) {
            BeamError::Raise(raise) => assert!(raise.stacktrace.is_empty()),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    /// An **engine diagnostic** (a [`crate::error::unsupported`] raise, whose reason
    /// atom starts `unsupported_opcode: `) is never decorated with frames, even when
    /// the machine's live frames *could* name one: the interpreter and the native
    /// baseline tier reach such a diagnostic through different paths, so frames here
    /// would make the two tiers disagree on the same error's `Debug`.
    #[test]
    fn with_uncaught_frames_leaves_engine_diagnostics_unframed() {
        let entry = module_with("m", &[("f", 1), ("g", 2)]);
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(Continuation::Address {
                func: 0,
                pc: 3,
                context: 0,
            })
            .unwrap();

        // The very frames the funnel *would* have named for a program-level raise.
        assert_eq!(machine.raise_stacktrace(Some(&entry), 1, 7, None).len(), 2);

        // A diagnostic is returned byte-identically: same reason, empty trace.
        let diagnostic = error::unsupported("deallocate: frame size mismatch");
        match machine.with_uncaught_frames(diagnostic, Some(&entry), 1, 7) {
            BeamError::Raise(raise) => {
                assert_eq!(
                    raise.reason.as_atom(),
                    Some("unsupported_opcode: deallocate: frame size mismatch")
                );
                assert!(raise.stacktrace.is_empty(), "diagnostics stay frame-less");
            }
            other => panic!("expected a raise, got {other:?}"),
        }
        // `error::blocked()` is the same sentinel and is excluded too.
        match machine.with_uncaught_frames(error::blocked(), Some(&entry), 1, 7) {
            BeamError::Raise(raise) => assert!(raise.stacktrace.is_empty()),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    #[test]
    fn throw_with_trace_leaves_the_frames_empty() {
        // The raise *site* attaches no frames: the funnel
        // (`Machine::with_uncaught_frames`) is what derives them, at the instruction
        // / driver boundary.
        let mut machine = Machine::new();
        let err = machine
            .throw(ExceptionClass::Throw, Eterm::from_raw(0x99))
            .expect_err("no catch frame");
        match err {
            BeamError::Raise(raise) => assert!(raise.stacktrace.is_empty()),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    #[test]
    fn a_caught_raise_lands_unchanged_and_never_exposes_the_frames() {
        // A raise that lands in a live catch frame is an `Ok(Step)`, so the frames the
        // funnel would derive never reach it: the landing registers and the catch
        // frame are exactly what they were before the frames existed.
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(9, 9, 0));
        let class_atom = make_atom(Atom::from_raw(11));
        machine.set_class_atom(ExceptionClass::Error, class_atom);
        let reason = Eterm::from_raw(0x1234);

        let step = machine
            .throw_with_trace(ExceptionClass::Error, reason, NIL)
            .expect("a live frame catches");
        assert_eq!(step, Step::Goto { func: 9, pc: 9 });
        // The C landing convention is untouched by the frames: x0 = THE_NON_VALUE,
        // x1 = reason, x2 = the caller's raw trace, x3 = the class atom.
        assert_eq!(machine.x(0), Some(Eterm::non_value()));
        assert_eq!(machine.x(1), Some(reason));
        assert_eq!(machine.x(2), Some(NIL));
        assert_eq!(machine.x(3), Some(class_atom));
        assert_eq!(machine.catch_frame_depth(), 1, "the handler pops it");
    }

    /// `pending_exception_mut` is the write half of the `pending_exception` pair: it
    /// hands out a reference into the machine's own slot, so a relocated reason term    /// written through it is what the read accessor observes next.
    #[test]
    fn pending_exception_mut_writes_the_same_slot_the_reader_returns() {
        // A fresh machine has no pending exception: the write half is `None` too.
        let mut fresh = Machine::new();
        assert!(fresh.pending_exception_mut().is_none());
        assert_eq!(fresh.pending_exception(), None);

        // Record a raise (the whole crate's entry point: `Machine::throw`).
        let mut machine = Machine::new();
        let reason = Eterm::from_raw(0xBEEF);
        assert!(matches!(
            machine.throw(ExceptionClass::Throw, reason),
            Err(BeamError::Raise(_))
        ));
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Throw, reason))
        );

        // Write a relocated `(class, reason)` pair through the mutable accessor.
        let relocated = Eterm::from_raw(0xCAFE);
        let slot = machine
            .pending_exception_mut()
            .expect("a raise was recorded");
        assert_eq!(*slot, (ExceptionClass::Throw, reason));
        slot.0 = ExceptionClass::Exit;
        slot.1 = relocated;

        // The reader observes the change: it is the same slot, not a copy.
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Exit, relocated))
        );
        assert_eq!(machine.pending_class(), Some(ExceptionClass::Exit));
    }

    // ---------------------------------------------------------------------
    // `ensure_class_atoms` — the class-atom choke point (live namespace)
    // ---------------------------------------------------------------------

    /// A live namespace: the module's load-time snapshot plus `names`, interned
    /// afterwards (so their ids are beyond the snapshot).
    fn namespace_with(module: &Module, names: &[&str]) -> beam_term::AtomTable {
        let mut live = module.atom_table.clone();
        for name in names {
            live.intern(name);
        }
        live
    }

    #[test]
    fn ensure_class_atoms_resolves_the_three_classes_from_the_live_namespace() {
        // W3: the module names none of the class atoms; the host's namespace names all
        // three. After the one-shot resolution a class-`error` raise lands instead of
        // being abandoned with a typed `unsupported`.
        let module = Module::default();
        assert!(module.atom_table.lookup("error").is_none());
        let live = namespace_with(&module, &["error", "exit", "throw"]);
        let error = live.lookup("error").expect("the namespace names error");
        let exit = live.lookup("exit").expect("the namespace names exit");
        let throw = live.lookup("throw").expect("the namespace names throw");

        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(2, 5, 0));
        let mut ctx = crate::context::MinimalContext::with_shared_atoms(live);

        machine.ensure_class_atoms(&mut ctx, &module);
        let reason = Eterm::from_raw(0x77);
        let step = machine
            .throw_with_trace(ExceptionClass::Error, reason, NIL)
            .expect("the class atom came from the namespace");
        assert_eq!(step, Step::Goto { func: 2, pc: 5 });
        assert_eq!(machine.x(3), Some(make_atom(error)), "x(3) = the class atom");

        // The other two classes are installed too (never substituted by other atoms).
        let mut exit_machine = Machine::new();
        exit_machine.ensure_class_atoms(&mut ctx, &module);
        exit_machine.push_frame(0).unwrap();
        exit_machine.push_catch_frame(CatchFrame::new(0, 1, 0));
        exit_machine
            .throw(ExceptionClass::Exit, Eterm::from_raw(0x88))
            .expect("class `exit` is nameable too");
        assert_eq!(exit_machine.x(3), Some(make_atom(exit)));
        let mut throw_machine = Machine::new();
        throw_machine.ensure_class_atoms(&mut ctx, &module);
        throw_machine.push_frame(0).unwrap();
        throw_machine.push_catch_frame(CatchFrame::new(0, 2, 0));
        throw_machine
            .throw(ExceptionClass::Throw, Eterm::from_raw(0x99))
            .expect("class `throw` is nameable too");
        assert_eq!(throw_machine.x(3), Some(make_atom(throw)));
    }

    #[test]
    fn ensure_class_atoms_never_overwrites_an_installed_atom_and_survives_a_reset() {
        // A class atom a raise site already installed (from the same namespace, so the
        // same index) is left alone, and the per-run latch is invalidated by `reset` —
        // a stale `true` would make the next run skip the resolution its own namespace
        // needs.
        let module = Module::default();
        let live = namespace_with(&module, &["error", "exit", "throw"]);
        let error = live.lookup("error").expect("the namespace names error");
        let mut ctx = crate::context::MinimalContext::with_shared_atoms(live);

        let mut machine = Machine::new();
        let installed = make_atom(Atom::from_raw(1));
        machine.set_class_atom(ExceptionClass::Error, installed);
        machine.ensure_class_atoms(&mut ctx, &module);
        // Not overwritten.
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        machine
            .throw_error(Eterm::from_raw(0x11))
            .expect("a live frame catches");
        assert_eq!(machine.x(3), Some(installed));

        // A reset invalidates the latch *and* the installed atoms, so the next run
        // resolves them again rather than skipping the resolution.
        machine.reset();
        assert!(!machine.class_atoms_live, "reset clears the per-run latch");
        machine.ensure_class_atoms(&mut ctx, &module);
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 1, 0));
        machine
            .throw_error(Eterm::from_raw(0x12))
            .expect("the namespace is consulted again");
        assert_eq!(machine.x(3), Some(make_atom(error)));
    }
}