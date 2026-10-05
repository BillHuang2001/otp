//! The machine core: the process-state registers, Erlang stack and call frames.
//!
//! [`Machine`] owns the **x-register file**, the **Erlang stack** (Y slots) and the
//! **call-frame stack**. The frame model reproduces the C interpreter exactly
//! (`../../erts/emulator/beam/emu/macros.tab`): an `allocate` of `stack_need`
//! Y slots pushes a frame whose *continuation-pointer slot* is modeled as a typed
//! [`Continuation`] rather than a raw stack word, and whose Y slots are the stack
//! words above it.
//!
//! The interpreter dispatch loop (`crate::interpreter`, `crate::operand`) is the
//! manager's deliverable; the register/stack primitives here (`set_x`,
//! `push_frame`, `pop_frame`) are its building blocks.

use beam_common::Result;
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;

/// X-register file size (the `beam_emu.c` `MAX_REG` bound).
pub const MAX_REGISTER: u32 = 1024;

/// Where a frame returns to.
///
/// Internal to the crate: the interpreter stores this in each frame's
/// continuation-pointer slot instead of a raw stack word. Used by the manager when
/// the dispatch loop lands.
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) struct Frame {
    /// The continuation this frame returns to.
    pub(crate) continuation: Continuation,
    /// Index into the machine stack of the frame's first Y slot.
    pub(crate) y_base: usize,
}

/// Process-state core: x-register file, Erlang stack and call-frame stack.
pub struct Machine {
    /// The X-register file (`x[0..max_register)`); the engine's argument/result file.
    pub(crate) x: Vec<Eterm>,
    /// The Erlang stack: the Y-slot words of every live frame, oldest first.
    pub(crate) stack: Vec<Eterm>,
    /// The call-frame stack (innermost last).
    pub(crate) frames: Vec<Frame>,
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
            stack: Vec::new(),
            frames: Vec::new(),
            max_register,
        };
        machine.reset();
        machine
    }

    /// Run the exported `function/arity` to its `return`; yields `x(0)`.
    ///
    /// Resolves the export entry label and delegates to [`Machine::run_label`]. The
    /// execution itself is the manager's deliverable; the skeleton returns the
    /// documented coverage-limit error.
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
    /// Seeds `x[0..arity)` from `args`, pushes the bottom frame, and enters the
    /// dispatch loop at `label`. The loop is the manager's deliverable; the skeleton
    /// returns the documented coverage-limit error.
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
        // The interpreter resolves `label` to a (function index, code offset); the
        // skeleton passes the function index `0` and the label as the offset.
        self.run_loop(module, 0, label.0, ctx)
    }

    /// Read X register `n`, or `None` if it is outside the register file.
    pub fn x(&self, n: u32) -> Option<Eterm> {
        self.x.get(n as usize).copied()
    }

    /// The number of call frames currently on the frame stack.
    pub fn frame_depth(&self) -> usize {
        self.frames.len()
    }

    /// The number of words currently on the Erlang stack.
    pub fn stack_depth(&self) -> usize {
        self.stack.len()
    }

    /// Reset to the initial state: a fresh (NIL-filled) register file and empty
    /// stack and frame stacks.
    pub fn reset(&mut self) {
        self.x.clear();
        self.x.resize(self.max_register as usize, Eterm::default());
        self.stack.clear();
        self.frames.clear();
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

    /// Push a frame reserving `y_count` Y slots (above the continuation-pointer slot).
    pub(crate) fn push_frame(&mut self, y_count: u32) -> Result<()> {
        let y_base = self.stack.len();
        self.stack.resize(y_base + y_count as usize, Eterm::default());
        self.frames.push(Frame {
            continuation: Continuation::Halt,
            y_base,
        });
        Ok(())
    }

    /// Pop the innermost frame (and its Y slots).
    ///
    /// Consumed by the interpreter dispatch loop once it lands (the manager); kept
    /// as a documented building block for the skeleton.
    #[allow(dead_code)]
    pub(crate) fn pop_frame(&mut self) -> Result<()> {
        match self.frames.pop() {
            Some(frame) => {
                self.stack.truncate(frame.y_base);
                Ok(())
            }
            None => Err(error::unsupported("pop_frame: no frame")),
        }
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
    fn run_label_seeds_registers_and_reports_unsupported() {
        let module = Module::default();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let args = [Eterm::from_raw(1), Eterm::from_raw(2)];
        let err = machine.run_label(&module, Label(1), 2, &args, &mut ctx);
        // Stub loop: unsupported, but the x-register seeding already happened.
        assert!(err.is_err());
        assert_eq!(machine.x(0), Some(Eterm::from_raw(1)));
        assert_eq!(machine.x(1), Some(Eterm::from_raw(2)));
    }
}
