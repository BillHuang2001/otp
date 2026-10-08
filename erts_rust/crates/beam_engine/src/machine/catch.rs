//! The exception scaffolding: the catch-frame stack and the raise/unwind entries.
//!
//! `try`/`catch` push a [`CatchFrame`]; a raise consults the innermost one and
//! either lands the exception in the handler registers (`x(0..=3)`, the C
//! `handle_error` convention) or propagates as a typed
//! [`BeamError::Raise`]. [`Machine::throw_with_trace`] is the **one uniform
//! raise/unwind entry point** every instruction family calls
//! (`crate::families::exceptions` owns the handler opcodes that pop the frame).

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise, Result};
use beam_term::{is_non_value, Eterm, NIL, THE_NON_VALUE};

use crate::error;
use crate::Step;

use super::Machine;

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

    /// The installed atom for `class`, or a typed error when unset/unknown.
    ///
    /// A missing atom means the class atom is absent from the running module's atom
    /// table. The engine (correctly) refuses to substitute a different atom: the
    /// class term would then be a *different, wrong* atom rather than the
    /// documented `unsupported` error.
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
    /// A raise is *not* a bare `BeamError` — it is a control-flow decision:
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
    /// A family body typically ends with `machine.throw(class, reason).map(Some)`
    /// (or, for a `{badmatch,V}`-style raise, [`Machine::throw_error`]).
    pub(crate) fn throw_with_trace(
        &mut self,
        class: ExceptionClass,
        reason: Eterm,
        trace: Eterm,
    ) -> Result<Step> {
        self.pending_exception = Some((class, reason));
        let frame = match self.catch_frames.last() {
            Some(frame) => *frame,
            // An uncaught structured raise carries its live reason term out of the
            // machine in the raise's `live_reason` slot; `ExitReason::Structured`
            // keeps `as_atom()` at `None`.
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
    use beam_common::{BeamError, ExceptionClass};
    use beam_loader::Atom;
    use beam_term::{make_atom, Eterm, NIL};

    use crate::Step;

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
}
