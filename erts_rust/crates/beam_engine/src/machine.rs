//! The machine core: the process-state registers, Erlang stack and call frames.
//!
//! [`Machine`] owns the **x-register file**, the **Erlang stack** (Y slots) and the
//! **call-frame stack**. The frame model reproduces the C interpreter exactly
//! (`../../erts/emulator/beam/emu/macros.tab`): an `allocate` of `stack_need`
//! Y slots pushes a frame whose *continuation-pointer slot* is modeled as a typed
//! [`Continuation`] rather than a raw stack word, and whose Y slots are the stack
//! words above it.
//!
//! The dispatch loop lives in [`crate::interpreter`]; the register/stack/frame
//! primitives here (`set_x`/`y`/`push_frame`/`pop_frame`/`pop_frame_checked`/
//! `trim`/`continuation`) are its building blocks.

use beam_common::{BeamError, ExceptionClass, ExitReason, Raise, Result};
use beam_loader::{Atom, Label, Module};
use beam_term::{Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter;
use crate::Step;

/// X-register file size (the `beam_emu.c` `MAX_REG` bound).
pub const MAX_REGISTER: u32 = 1024;

/// Float-register (FR) file size.
///
/// The C runtime's float-register file is
/// `union { FloatDef d[MAX_REG]; ... } f_reg_array`
/// (`../../erts/emulator/beam/erl_process.h`) with `#define MAX_REG 1024`
/// (`../../erts/emulator/beam/erl_vm.h:49`), so the FR file mirrors the real FR
/// count (1024) — the same bound as [`MAX_REGISTER`]. The C JIT bounds the index
/// to `0..=1023` (`../../erts/emulator/beam/jit/x86/beam_asm.hpp`).
pub const MAX_FLOAT_REGISTER: u32 = 1024;

/// Where a frame returns to.
///
/// The interpreter stores this in each frame's continuation-pointer slot instead
/// of a raw stack word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Continuation {
    /// Return to code offset `pc` in the function at index `func`.
    Address {
        /// The function index (into [`Module::functions`]).
        func: u32,
        /// The code offset within that function.
        pc: u32,
    },
    /// The bottom frame: returning halts execution and yields `x(0)`.
    Halt,
}

/// One call frame: the continuation-pointer slot plus the base of its Y slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The continuation this frame returns to.
    pub(crate) continuation: Continuation,
    /// Index into the machine stack of the frame's first Y slot.
    pub(crate) y_base: usize,
    /// The number of Y slots this frame owns (Y slots `y_base .. y_base + y_count`).
    pub(crate) y_count: u32,
}

/// One entry of the machine's exception catch-frame stack.
///
/// `try Frame Fail` / `catch Frame Fail` push one of these (the handler's
/// `(func, pc)` target plus the catch-frame term the program keeps in `frame`);
/// `try_end`/`catch_end` pop it. The exception layer
/// ([`crate::families::exceptions`]) consults the stack to turn a raise into a
/// `Step::Goto` to the handler, without touching `crate::interpreter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatchFrame {
    /// The function index (into [`Module::functions`]) of the handler.
    pub func: u32,
    /// The code offset of the handler within that function.
    pub pc: u32,
    /// The catch-frame term the handler reads from its `Frame` operand.
    pub frame: Eterm,
}

impl CatchFrame {
    /// A catch frame resuming at `pc` in the function at index `func`, carrying
    /// the catch-frame term `frame`.
    pub fn new(func: u32, pc: u32, frame: Eterm) -> CatchFrame {
        CatchFrame { func, pc, frame }
    }
}

/// Process-state core: x-register file, Erlang stack and call-frame stack.
pub struct Machine {
    /// The X-register file (`x[0..max_register)`); the engine's argument/result file.
    pub(crate) x: Vec<Eterm>,
    /// The float-register (FR) file (`fr[0..MAX_FLOAT_REGISTER)`).
    ///
    /// At Milestone 1 this stores `Eterm` term slots — float registers hold terms,
    /// uniformly with the x-register file — so the existing [`crate::operand`]
    /// read/write seam (which returns/takes `Eterm`) can service `Operand::FloatReg`.
    /// The faithful raw-double `FloatDef` model (`f_reg_array`) is a **deferred
    /// refinement**; the field deliberately does **not** store `f64`.
    pub(crate) fr: Vec<Eterm>,
    /// The Erlang stack: the Y-slot words of every live frame, oldest first.
    pub(crate) stack: Vec<Eterm>,
    /// The call-frame stack (innermost last).
    pub(crate) frames: Vec<Frame>,
    /// The exception catch-frame stack (innermost last).
    ///
    /// Pushed by `try`/`catch`, popped by `try_end`/`catch_end` (see
    /// [`CatchFrame`]); empty until the exception family executes those opcodes.
    pub(crate) catch_frames: Vec<CatchFrame>,
    /// The pending exception `(class, reason)` recorded by [`Machine::throw`],
    /// if any.
    pub(crate) pending_exception: Option<(ExceptionClass, Eterm)>,
    /// The register-file size (a configurable bound, default [`MAX_REGISTER`]).
    pub(crate) max_register: u32,
}

impl Machine {
    /// A machine with the default register file ([`MAX_REGISTER`] registers).
    pub fn new() -> Machine {
        Machine::with_max_register(MAX_REGISTER)
    }

    /// A machine whose register file has `max_register` registers.
    pub fn with_max_register(max_register: u32) -> Machine {
        let mut machine = Machine {
            x: Vec::new(),
            fr: Vec::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            catch_frames: Vec::new(),
            pending_exception: None,
            max_register,
        };
        machine.reset();
        machine
    }

    /// Run the exported `function/arity` to its `return`; yields `x(0)`.
    ///
    /// Resolves the export entry label and delegates to [`Machine::run_label`].
    pub fn run_export(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        match module.export(function, arity) {
            Some(label) => self.run_label(module, label, arity, args, ctx),
            None => Err(error::unsupported("run_export: export not found")),
        }
    }

    /// Run a raw entry label (used by [`Machine::run_export`]).
    ///
    /// Resets the machine, seeds `x[0..arity)` from `args` (the remaining
    /// registers stay `NIL`), pushes the **bottom** frame (whose continuation is
    /// [`Continuation::Halt`]), resolves `label` to its `(function, code)` pair
    /// and enters the dispatch loop.
    pub fn run_label(
        &mut self,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        self.reset();
        let n = arity.min(args.len() as u32);
        for (i, arg) in args.iter().take(n as usize).enumerate() {
            self.set_x(i as u32, *arg)?;
        }
        self.push_frame(0)?;
        let (func, pc) = interpreter::resolve_label(module, label)?;
        self.run_loop(module, func, pc, ctx)
    }

    /// Read X register `n`, or `None` if it is outside the register file.
    pub fn x(&self, n: u32) -> Option<Eterm> {
        self.x.get(n as usize).copied()
    }

    /// Read float register `n`, or `None` if it is outside the FR file.
    ///
    /// Mirrors [`Machine::x`]: a bounds-checked read of the FR file.
    pub fn get_fr(&self, n: u32) -> Option<Eterm> {
        self.fr.get(n as usize).copied()
    }

    /// Write `value` into float register `n`.
    ///
    /// Mirrors [`Machine::set_x`]: returns [`crate::error::bad_register`] if `n` is
    /// outside the FR file.
    pub fn set_fr(&mut self, n: u32, value: Eterm) -> Result<()> {
        match self.fr.get_mut(n as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(error::bad_register(n)),
        }
    }

    /// Read Y slot `k` of the **current** frame, or `None` if there is no frame or
    /// `k` is outside the frame's Y slots.
    pub(crate) fn y(&self, k: u32) -> Option<Eterm> {
        let frame = self.frames.last()?;
        if k >= frame.y_count {
            return None;
        }
        self.stack.get(frame.y_base + k as usize).copied()
    }

    /// The number of call frames currently on the frame stack.
    pub fn frame_depth(&self) -> usize {
        self.frames.len()
    }

    /// The number of words currently on the Erlang stack.
    pub fn stack_depth(&self) -> usize {
        self.stack.len()
    }

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

    /// Reset to the initial state: a fresh (NIL-filled) register file and empty
    /// stack and frame stacks.
    pub fn reset(&mut self) {
        self.x.clear();
        self.x.resize(self.max_register as usize, Eterm::default());
        self.fr.clear();
        self.fr
            .resize(MAX_FLOAT_REGISTER as usize, Eterm::default());
        self.stack.clear();
        self.frames.clear();
        self.catch_frames.clear();
        self.pending_exception = None;
    }

    /// Write `v` into X register `n`.
    ///
    /// Returns [`crate::error::bad_register`] if `n` is outside the register file.
    pub(crate) fn set_x(&mut self, n: u32, v: Eterm) -> Result<()> {
        match self.x.get_mut(n as usize) {
            Some(slot) => {
                *slot = v;
                Ok(())
            }
            None => Err(error::bad_register(n)),
        }
    }

    /// Write `v` into Y slot `k` of the current frame.
    ///
    /// Returns [`crate::error::bad_register`] if there is no frame or `k` is
    /// outside the frame's Y slots.
    pub(crate) fn set_y(&mut self, k: u32, v: Eterm) -> Result<()> {
        let (y_base, y_count) = match self.frames.last() {
            Some(frame) => (frame.y_base, frame.y_count),
            None => return Err(error::bad_register(k)),
        };
        if k >= y_count {
            return Err(error::bad_register(k));
        }
        match self.stack.get_mut(y_base + k as usize) {
            Some(slot) => {
                *slot = v;
                Ok(())
            }
            None => Err(error::bad_register(k)),
        }
    }

    /// Ensure `heap_need` heap words are available, handing the context the exact
    /// root set (`x[0..live)` plus the whole stack) so a Stage-4 collector can move
    /// terms in place.
    pub(crate) fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<()> {
        let (x, stack) = (&mut self.x, &mut self.stack);
        ctx.ensure_heap(heap_need, live, x, stack)
    }

    /// The current frame's continuation.
    pub(crate) fn continuation(&self) -> Result<Continuation> {
        match self.frames.last() {
            Some(frame) => Ok(frame.continuation),
            None => Err(error::unsupported("run: no current frame")),
        }
    }

    /// Overwrite the current frame's continuation-pointer slot.
    pub(crate) fn set_continuation(&mut self, continuation: Continuation) -> Result<()> {
        match self.frames.last_mut() {
            Some(frame) => {
                frame.continuation = continuation;
                Ok(())
            }
            None => Err(error::unsupported("run: no current frame")),
        }
    }

    /// The halted frame's result: `x(0)`.
    pub(crate) fn finish(&self) -> Result<Eterm> {
        self.x(0).ok_or_else(|| error::bad_register(0))
    }

    /// Push a frame reserving `y_count` Y slots (above the continuation-pointer slot).
    pub(crate) fn push_frame(&mut self, y_count: u32) -> Result<()> {
        let y_base = self.stack.len();
        self.stack
            .resize(y_base + y_count as usize, Eterm::default());
        self.frames.push(Frame {
            continuation: Continuation::Halt,
            y_base,
            y_count,
        });
        Ok(())
    }

    /// Pop the innermost frame (and its Y slots).
    pub(crate) fn pop_frame(&mut self) -> Result<()> {
        match self.frames.pop() {
            Some(frame) => {
                self.stack.truncate(frame.y_base);
                Ok(())
            }
            None => Err(error::unsupported("pop_frame: no frame")),
        }
    }

    /// Pop the innermost frame after checking that it owns exactly `n` Y slots.
    ///
    /// Used by `deallocate`/`call_last`/`call_ext_last`; a mismatched size is a
    /// typed error, never a silent stack corruption.
    pub(crate) fn pop_frame_checked(&mut self, n: u32) -> Result<()> {
        let y_count = match self.frames.last() {
            Some(frame) => frame.y_count,
            None => return Err(error::unsupported("deallocate: no frame")),
        };
        if y_count != n {
            return Err(error::unsupported("deallocate: frame size mismatch"));
        }
        self.pop_frame()
    }

    /// `trim N` — drop the current frame's top `n` Y slots.
    pub(crate) fn trim(&mut self, n: u32) -> Result<()> {
        let n_usize = n as usize;
        match self.frames.last_mut() {
            Some(frame) => {
                if n > frame.y_count {
                    return Err(error::unsupported("trim: n exceeds frame size"));
                }
                frame.y_count -= n;
            }
            None => return Err(error::unsupported("trim: no frame")),
        }
        if n_usize > self.stack.len() {
            return Err(error::unsupported("trim: stack underflow"));
        }
        let new_len = self.stack.len() - n_usize;
        self.stack.truncate(new_len);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Exception scaffolding (the shared state the `families::exceptions` bodies use)
    // -----------------------------------------------------------------------

    /// Push a catch frame (the `try`/`catch` handler target).
    pub fn push_catch_frame(&mut self, frame: CatchFrame) {
        self.catch_frames.push(frame);
    }

    /// Pop the innermost catch frame, if any.
    pub fn pop_catch_frame(&mut self) -> Option<CatchFrame> {
        self.catch_frames.pop()
    }

    /// Raise a BEAM exception with `reason` of class `class`.
    ///
    /// **This is the one uniform raise/unwind entry point** every instruction
    /// family calls (the wave-2 contract); it replaces the earlier
    /// `raise`/`raise_with -> BeamError` pair. A raise is *not* a bare `BeamError`
    /// — it is a control-flow decision:
    ///
    /// 1. The pending exception `(class, reason)` is recorded on the machine
    ///    ([`Machine::pending_exception`]). `beam_common::BeamError` cannot carry a
    ///    *compound* (tuple) reason — its [`ExitReason::Structured`] variant holds no
    ///    term — so this field is where the live reason survives.
    /// 2. If a catch frame is live ([`Machine::catch_frame_depth`] `> 0`) the
    ///    **innermost** one is *popped* and the exception is landed in the handler
    ///    registers ([`Machine::land_exception`]); the result is
    ///    `Ok(Step::Goto { func, pc })` for that handler.
    /// 3. If no catch frame is live the result is the typed
    ///    `Err(BeamError::Raise(..))` the run propagates — **never a panic**.
    ///
    /// A family body typically ends with `machine.throw(class, reason).map(Some)`
    /// (or, for a `{badmatch,V}`-style raise, [`Machine::throw_error`]).
    #[allow(dead_code)] // the wave-2 exception family calls this; no in-crate caller yet
    pub(crate) fn throw(&mut self, class: ExceptionClass, reason: Eterm) -> Result<Step> {
        self.pending_exception = Some((class, reason));
        match self.catch_frames.pop() {
            Some(frame) => {
                self.land_exception(reason)?;
                Ok(Step::Goto {
                    func: frame.func,
                    pc: frame.pc,
                })
            }
            None => Err(BeamError::Raise(Raise::new(class, ExitReason::Structured))),
        }
    }

    /// [`Machine::throw`] with class [`ExceptionClass::Error`] — the convenience
    /// for the `{badmatch,V}`/`{case_clause,V}`/`{badrecord,V}`-style raises the
    /// exception family emits.
    #[allow(dead_code)] // see `throw`: built for the wave-2 exception family
    pub(crate) fn throw_error(&mut self, reason: Eterm) -> Result<Step> {
        self.throw(ExceptionClass::Error, reason)
    }

    /// Land a *caught* exception in the handler registers.
    ///
    /// Reproduces the C convention established before `next_catch` jumps to the
    /// handler (`../../erts/emulator/beam/beam_common.c:541-544`, `handle_error`):
    ///
    /// * `x(0) = THE_NON_VALUE` — the "the previous code completed abnormally"
    ///   marker that `catch_end`/`try_case` test (`emu/instrs.tab:1175-1227`);
    /// * `x(1) = reason`;
    /// * `x(2) = stacktrace` — Milestone 1 captures no stacktrace, so `[]`;
    /// * `x(3) = exception_tag[class]` — the class atom. **Milestone-1
    ///   substitution:** C reads this from its global `am_error`/`am_exit`/
    ///   `am_throw` constants, but [`Machine`] owns no atom table, so `x(3)` is
    ///   filled with `[]` and the class is instead carried in
    ///   [`Machine::pending_exception`]; the handler (`try_case`/`catch_end`, the
    ///   exceptions family — which has the [`Module`]) resolves the class atom into
    ///   `x(3)` from there. This mirrors the C behavior exactly, because `x(3)` is
    ///   only ever read by that handler.
    fn land_exception(&mut self, reason: Eterm) -> Result<()> {
        self.set_x(0, Eterm::non_value())?;
        self.set_x(1, reason)?;
        self.set_x(2, NIL)?;
        self.set_x(3, NIL)?;
        Ok(())
    }
}

impl Default for Machine {
    /// Same as [`Machine::new`]: the default register file ([`MAX_REGISTER`]).
    fn default() -> Machine {
        Machine::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use beam_loader::Label;

    #[test]
    fn new_has_nil_filled_register_file() {
        let machine = Machine::new();
        assert_eq!(machine.x.len(), MAX_REGISTER as usize);
        assert_eq!(machine.x(0), Some(Eterm::default()));
        assert_eq!(machine.x(MAX_REGISTER - 1), Some(Eterm::default()));
        assert_eq!(machine.x(MAX_REGISTER), None);
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
    }

    #[test]
    fn with_max_register_bounds_the_file() {
        let machine = Machine::with_max_register(4);
        assert_eq!(machine.x, vec![Eterm::default(); 4]);
    }

    #[test]
    fn new_has_nil_filled_float_register_file() {
        let machine = Machine::new();
        assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
        assert_eq!(machine.get_fr(0), Some(Eterm::default()));
        assert_eq!(
            machine.get_fr(MAX_FLOAT_REGISTER - 1),
            Some(Eterm::default())
        );
        assert_eq!(machine.get_fr(MAX_FLOAT_REGISTER), None);
    }

    #[test]
    fn set_fr_get_fr_round_trip_in_range() {
        let mut machine = Machine::new();
        let v = Eterm::from_raw(0x5F5F);
        assert!(machine.set_fr(10, v).is_ok());
        assert_eq!(machine.get_fr(10), Some(v));
        assert_eq!(machine.get_fr(11), Some(Eterm::default()));
    }

    #[test]
    fn fr_access_is_bounds_checked() {
        let mut machine = Machine::new();
        assert_eq!(machine.get_fr(MAX_FLOAT_REGISTER), None);
        let err = machine.set_fr(MAX_FLOAT_REGISTER, Eterm::from_raw(1));
        assert!(matches!(err, Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    fn reset_clears_a_dirty_float_register() {
        let mut machine = Machine::new();
        machine.set_fr(3, Eterm::from_raw(0x77)).unwrap();
        machine.set_x(0, Eterm::from_raw(0x88)).unwrap();
        machine.reset();
        assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
        assert_eq!(machine.get_fr(3), Some(Eterm::default()));
        // The x-register behavior is untouched by the FR reset.
        assert_eq!(machine.x.len(), MAX_REGISTER as usize);
        assert_eq!(machine.x(0), Some(Eterm::default()));
    }

    #[test]
    fn set_x_checks_the_bound() {
        let mut machine = Machine::with_max_register(4);
        let v = Eterm::from_raw(0x2A);
        assert!(machine.set_x(3, v).is_ok());
        assert_eq!(machine.x(3), Some(v));
        assert!(machine.set_x(4, v).is_err());
    }

    #[test]
    fn push_and_pop_frames_track_the_stack() {
        let mut machine = Machine::new();
        assert!(machine.push_frame(2).is_ok());
        assert_eq!(machine.frame_depth(), 1);
        assert_eq!(machine.stack_depth(), 2);
        assert!(machine.push_frame(1).is_ok());
        assert_eq!(machine.frame_depth(), 2);
        assert_eq!(machine.stack_depth(), 3);
        assert!(machine.pop_frame().is_ok());
        assert_eq!(machine.frame_depth(), 1);
        assert_eq!(machine.stack_depth(), 2);
        // Popping the last frame empties the stack.
        assert!(machine.pop_frame().is_ok());
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
        // Popping past the bottom is a typed error, not a panic.
        assert!(machine.pop_frame().is_err());
    }

    #[test]
    fn y_slots_are_scoped_to_the_current_frame() {
        let mut machine = Machine::new();
        machine.push_frame(2).unwrap();
        machine.set_y(0, Eterm::from_raw(0xAA)).unwrap();
        machine.set_y(1, Eterm::from_raw(0xBB)).unwrap();
        assert_eq!(machine.y(0), Some(Eterm::from_raw(0xAA)));
        assert_eq!(machine.y(1), Some(Eterm::from_raw(0xBB)));
        // Out of the frame's Y slots.
        assert_eq!(machine.y(2), None);
        assert!(machine.set_y(2, Eterm::default()).is_err());

        // A second frame sees only its own slots.
        machine.push_frame(1).unwrap();
        assert_eq!(machine.y(0), Some(Eterm::default()));
        assert_eq!(machine.y(1), None);
        machine.set_y(0, Eterm::from_raw(0xCC)).unwrap();
        machine.pop_frame().unwrap();
        assert_eq!(machine.y(0), Some(Eterm::from_raw(0xAA)));
    }

    #[test]
    fn y_without_a_frame_is_none() {
        let machine = Machine::new();
        assert_eq!(machine.y(0), None);
    }

    #[test]
    fn pop_frame_checked_validates_the_size() {
        let mut machine = Machine::new();
        machine.push_frame(3).unwrap();
        assert!(machine.pop_frame_checked(2).is_err());
        assert_eq!(machine.frame_depth(), 1);
        assert!(machine.pop_frame_checked(3).is_ok());
        assert_eq!(machine.frame_depth(), 0);
        assert!(machine.pop_frame_checked(0).is_err());
    }

    #[test]
    fn trim_drops_top_y_slots() {
        let mut machine = Machine::new();
        machine.push_frame(4).unwrap();
        machine.set_y(0, Eterm::from_raw(1)).unwrap();
        machine.set_y(3, Eterm::from_raw(4)).unwrap();
        machine.trim(2).unwrap();
        assert_eq!(machine.stack_depth(), 2);
        assert_eq!(machine.y(0), Some(Eterm::from_raw(1)));
        assert_eq!(machine.y(2), None);
        assert!(machine.trim(3).is_err());
    }

    #[test]
    fn ensure_heap_forwards_the_root_set() {
        struct Probe(usize);
        impl ExecutionContext for Probe {
            fn call_bif(&mut self, _m: &str, _f: &str, _a: u32, _args: &[Eterm]) -> Result<Eterm> {
                Err(beam_common::BeamError::BadArg)
            }
            fn ensure_heap(
                &mut self,
                _need: u32,
                live: u32,
                x_regs: &mut [Eterm],
                stack: &mut [Eterm],
            ) -> Result<()> {
                self.0 = live as usize + x_regs.len() + stack.len();
                Ok(())
            }
        }
        let mut machine = Machine::with_max_register(8);
        machine.push_frame(2).unwrap();
        let mut ctx = Probe(0);
        machine.ensure_heap(16, 3, &mut ctx).unwrap();
        assert_eq!(ctx.0, 3 + 8 + 2);
    }

    #[test]
    fn reset_clears_state_and_refills_registers() {
        let mut machine = Machine::with_max_register(3);
        machine.set_x(0, Eterm::from_raw(7)).unwrap();
        machine.push_frame(2).unwrap();
        machine.reset();
        assert_eq!(machine.x, vec![Eterm::default(); 3]);
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
    }

    #[test]
    fn run_export_of_missing_export_is_a_typed_error() {
        let module = Module::default();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert!(machine
            .run_export(&module, Atom::MAX, 0, &[], &mut ctx)
            .is_err());
    }

    #[test]
    fn run_label_seeds_registers_then_reports_unknown_label() {
        let module = Module::default();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let args = [Eterm::from_raw(1), Eterm::from_raw(2)];
        // An empty module has no label 1: a typed error, but the register seeding
        // already happened and a bottom frame was pushed for the duration.
        let err = machine.run_label(&module, Label(1), 2, &args, &mut ctx);
        assert!(err.is_err());
        assert_eq!(machine.x(0), Some(Eterm::from_raw(1)));
        assert_eq!(machine.x(1), Some(Eterm::from_raw(2)));
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
    fn throw_to_a_catch_frame_lands_the_exception_and_pops_the_frame() {
        let mut machine = Machine::new();
        // Two live frames: only the innermost must be consumed.
        machine.push_catch_frame(CatchFrame::new(3, 7, Eterm::from_raw(0x11)));
        machine.push_catch_frame(CatchFrame::new(9, 9, Eterm::from_raw(0x22)));
        let reason = Eterm::from_raw(0x1234);

        let outcome = machine.throw_error(reason).expect("a live frame catches");
        assert_eq!(outcome, Step::Goto { func: 9, pc: 9 });
        assert_eq!(machine.catch_frame_depth(), 1);
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Error, reason))
        );

        // The C handler convention (beam_common.c:541-544): x0 = THE_NON_VALUE,
        // x1 = reason, x2 = [] stacktrace, x3 = [] class-atom placeholder.
        assert_eq!(machine.x(0), Some(Eterm::non_value()));
        assert_eq!(machine.x(1), Some(reason));
        assert_eq!(machine.x(2), Some(NIL));
        assert_eq!(machine.x(3), Some(NIL));
    }

    #[test]
    fn throw_returns_the_outer_frame_on_a_second_raise() {
        let mut machine = Machine::new();
        machine.push_catch_frame(CatchFrame::new(3, 7, Eterm::from_raw(0x11)));
        let _ = machine
            .throw(ExceptionClass::Exit, Eterm::from_raw(1))
            .unwrap();
        // The single frame is gone: the next raise cannot be caught.
        assert!(machine
            .throw(ExceptionClass::Exit, Eterm::from_raw(2))
            .is_err());
        assert_eq!(machine.catch_frame_depth(), 0);
    }
}
