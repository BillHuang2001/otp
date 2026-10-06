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

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise, Result};
use beam_loader::{Atom, Label, Module, SharedLiteralArea};
use beam_term::{is_non_value, Eterm, NIL, THE_NON_VALUE};

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter;
use crate::jit::Tier;
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
    /// The function index (into [`Module::functions`]) of the handler.
    pub func: u32,
    /// The code offset of the handler within that function.
    pub pc: u32,
    /// Index into [`Machine::frames`](Machine) of the call frame owning the
    /// handler; a raise unwinds the call-frame stack back to it (and drops its Y
    /// slots' descendants) before landing the exception.
    pub frame_index: usize,
}

impl CatchFrame {
    /// A catch frame whose handler is at `pc` in the function at index `func`,
    /// living in the call frame at `frame_index`.
    pub fn new(func: u32, pc: u32, frame_index: usize) -> CatchFrame {
        CatchFrame {
            func,
            pc,
            frame_index,
        }
    }
}

/// The outcome of one engine run: either it returned a value, or the process blocked
/// in a `receive` with no message available (and must be resumed at `func`/`pc`).
///
/// Produced by the blocking-aware entries [`Machine::run_export_outcome`]/
/// [`Machine::run_label_outcome`]/[`Machine::resume_outcome`]. The plain
/// [`Machine::run_export`]/[`Machine::run_label`] (and the JIT backends'
/// `run_export`) are **non-blocking**: they fold a [`RunOutcome::Wait`] into the
/// typed `Err(`[`crate::error::blocked`]`)` rather than inventing a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run finished; this is `x(0)`.
    Returned(Eterm),
    /// The process blocked on an empty `receive`; resume at code offset `pc` of the
    /// function at index `func` (into [`Module::functions`]) once a message arrives.
    Wait {
        /// The function index (into [`Module::functions`]) to resume in.
        func: u32,
        /// The code offset within that function to resume at.
        pc: u32,
    },
}

/// Fold a run's [`RunOutcome`] into the `Result<Eterm>` the non-blocking entries
/// return.
///
/// A [`RunOutcome::Returned`] is the value; a [`RunOutcome::Wait`] becomes the
/// typed [`crate::error::blocked`] error, because a non-blocking entry cannot
/// suspend the process and must never substitute a wrong result for the block.
fn fold_outcome(outcome: RunOutcome) -> Result<Eterm> {
    match outcome {
        RunOutcome::Returned(value) => Ok(value),
        RunOutcome::Wait { .. } => Err(error::blocked()),
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
    /// Pushed by `try`/`catch`, popped by the handler opcodes
    /// (`try_case`/`try_end`/`catch_end`) — see [`CatchFrame`].
    pub(crate) catch_frames: Vec<CatchFrame>,
    /// The pending exception `(class, reason)` recorded by [`Machine::throw`],
    /// if any.
    pub(crate) pending_exception: Option<(ExceptionClass, Eterm)>,
    /// The exception-class atoms (`error`, `exit`, `throw`) resolved from the
    /// running module's atom table, indexed by [`exception_class_index`];
    /// `THE_NON_VALUE` means "not resolved yet".
    ///
    /// [`Machine::land_exception`] places the class atom in `x(3)` exactly as C
    /// places `exception_tag[GET_EXC_CLASS(freason)]`
    /// (`../../erts/emulator/beam/beam_common.c:541-544`). C reads its
    /// `am_error`/`am_exit`/`am_throw` globals; the engine has no atom table, so
    /// the exceptions family installs the atoms it resolved from the
    /// [`Module`] before raising ([`Machine::set_class_atom`]) and the raise fails
    /// with a typed error when the class atom is missing (never a wrong atom).
    class_atoms: [Eterm; 3],
    /// The register-file size (a configurable bound, default [`MAX_REGISTER`]).
    pub(crate) max_register: u32,
    /// The static-literal area of the **module currently being executed**.
    ///
    /// Bound at the start of every run ([`Machine::run_loop`] clones the `Arc`
    /// out of the `&Module` being executed) and cleared by [`Machine::reset`].
    /// It lets the shared operand seam ([`crate::operand::read_operand`]) resolve
    /// `Operand::Literal(idx)` through [`Machine::literal`] without threading
    /// `&Module` through every call site.
    ///
    /// **Why the binding cannot go stale mid-run.** Within one
    /// [`Machine::run_export`]/[`Machine::run_label`] the executing module is
    /// fixed: labels are module-local and a cross-module call leaves the dispatch
    /// loop through `CallExt` into the host [`ExecutionContext`], which never
    /// re-enters the engine with a different `&Module` on the same run. A fresh
    /// `run_*` calls [`Machine::reset`] (clearing this) and re-binds it, so the
    /// area always matches the module being interpreted.
    ///
    /// A literal [`Eterm`] points into the area's **own** heap (never a process
    /// heap). At Milestone 1 there is no GC, so reading/comparing/copying a
    /// literal is safe; once the Stage-4 collector lands a literal must be treated
    /// as permanent (never scanned/copied as a process-heap term).
    literals: Option<SharedLiteralArea>,
    /// The **off-heap ref-counted binary payloads** this machine has built.
    ///
    /// A binary larger than [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`] is stored
    /// off the process heap: the on-heap term is the frozen 8-word
    /// `[BinRef][ErlSubBits]` pair whose `BinRef.val` word points at the payload
    /// ([`beam_term::refc`]). The payload itself is a [`beam_alloc::RefcBinary`]
    /// handle, and holding the handle here is what keeps the payload alive (and
    /// its address stable) for as long as the machine lives — dropping the machine
    /// releases every payload it built. See [`Machine::alloc_refc_binary`].
    ///
    /// **Never cleared by [`Machine::reset`].** A returned term may outlive the run
    /// that built it (`Machine::run_export` resets at the start of *every* run, and
    /// a host may consume the returned `Eterm` afterwards), so a reset must not free
    /// a payload a caller can still read.
    pub(crate) refc_binaries: Vec<beam_alloc::RefcBinary>,
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
            class_atoms: UNRESOLVED_CLASS_ATOMS,
            max_register,
            literals: None,
            refc_binaries: Vec::new(),
        };
        machine.reset();
        machine
    }

    /// Run the exported `function/arity` to its `return`; yields `x(0)`.
    ///
    /// Resolves the export entry label and delegates to [`Machine::run_label`].
    ///
    /// This is the **non-blocking** entry: a `receive` that finds no message
    /// available suspends the process, and because this entry cannot suspend it
    /// folds that outcome into the typed `Err(`[`crate::error::blocked`]`)` (never
    /// a panic and never an invented result). Use
    /// [`Machine::run_export_outcome`] + [`Machine::resume_outcome`] to drive a
    /// blocking receive.
    pub fn run_export(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        fold_outcome(self.run_export_outcome(module, function, arity, args, ctx)?)
    }

    /// Run the exported `function/arity` and report whether it returned or blocked.
    ///
    /// The export is resolved exactly as [`Machine::run_export`] does (a missing
    /// export stays the typed `unsupported("run_export: export not found")`); the
    /// difference is only the *outcome* type, so a `receive` that finds no message
    /// is reported as [`RunOutcome::Wait`] instead of an error.
    pub fn run_export_outcome(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        match module.export(function, arity) {
            Some(label) => self.run_label_outcome(module, label, arity, args, ctx),
            None => Err(error::unsupported("run_export: export not found")),
        }
    }

    /// Resume the **current** machine state at a [`RunOutcome::Wait`] address.
    ///
    /// The machine must be the one a previous outcome reported its `(func, pc)` for
    /// (typically from the same `ExecutionContext`'s mailbox): this entry
    /// deliberately does **not** [`reset`](Machine::reset) — the registers, the
    /// Erlang stack and every call frame of the suspended `receive` must survive so
    /// the process continues exactly where it blocked. It does (re)bind the
    /// module's literal area for the run, which is what
    /// [`Machine::run_label_outcome`]'s reset used to do; that is precisely
    /// [`Machine::run_loop`]'s body (literal binding + label map + dispatch loop,
    /// no reset), so this delegates to it.
    ///
    /// An out-of-range `(func, pc)` is the loop's own typed error, never a panic.
    pub fn resume_outcome(
        &mut self,
        module: &Module,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.run_loop(module, func, pc, ctx)
    }

    /// Run the exported `function/arity` on a selected execution tier; yields `x(0)`.
    ///
    /// The tier-selecting entry point of the engine ([`crate::jit::Tier`]): the
    /// caller states which tier it wants and gets that tier's result or a typed
    /// error — the engine never silently substitutes a different tier.
    ///
    /// * [`Tier::Interpreter`] — delegates to [`Machine::run_export`], which
    ///   remains the **default interpreter entry** (unchanged; this method is a
    ///   dispatcher, not a replacement).
    /// * [`Tier::BaselineJit`] — the native baseline backend
    ///   ([`crate::jit::baseline::BaselineJit`]): it compiles the module's basic
    ///   blocks to x86-64 and runs the export through a driver loop that falls back
    ///   to one interpreter step per un-lowered instruction.
    /// * [`Tier::Cranelift`] — the optional Cranelift hot tier
    ///   (`crate::jit::cranelift::CraneliftJit`), compiled in only with the
    ///   `jit-cranelift` Cargo feature: it lowers each IR function into a native
    ///   Cranelift function that loops over the function's control flow and delegates
    ///   per-instruction semantics to the same interpreter-backed helper. With the
    ///   feature off this is a typed [`unsupported`](crate::error::unsupported)
    ///   error, never a panic and never a fallback to the interpreter.
    pub fn run_with_tier(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
        tier: Tier,
    ) -> Result<Eterm> {
        match tier {
            Tier::Interpreter => self.run_export(module, function, arity, args, ctx),
            Tier::BaselineJit => {
                use crate::jit::JitBackend;
                let mut backend = crate::jit::baseline::BaselineJit::new();
                backend.run_export(self, module, function, arity, args, ctx)
            }
            Tier::Cranelift => crate::jit::run_cranelift(self, module, function, arity, args, ctx),
        }
    }

    /// Run a raw entry label (used by [`Machine::run_export`]).
    ///
    /// Resets the machine, seeds `x[0..arity)` from `args` (the remaining
    /// registers stay `NIL`), pushes the **bottom** frame (whose continuation is
    /// [`Continuation::Halt`]), resolves `label` to its `(function, code)` pair
    /// and enters the dispatch loop.
    ///
    /// Like [`Machine::run_export`] this is a **non-blocking** entry: a
    /// [`RunOutcome::Wait`] becomes the typed `Err(`[`crate::error::blocked`]`)`.
    /// Use [`Machine::run_label_outcome`] when the caller wants to see (and later
    /// resume) the suspension.
    pub fn run_label(
        &mut self,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        fold_outcome(self.run_label_outcome(module, label, arity, args, ctx)?)
    }

    /// Run a raw entry label and report whether it returned or blocked.
    ///
    /// The exact body of [`Machine::run_label`] with a [`RunOutcome`] result
    /// instead of the folded `x(0)`: the machine is reset, `x[0..arity)` is seeded
    /// from `args`, the bottom frame is pushed, and the dispatch loop runs from the
    /// label's `(func, pc)`.
    pub fn run_label_outcome(
        &mut self,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
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
        // The class atoms belong to the module being run, so a reset (the start of
        // every `run_*`) invalidates them; the exceptions family reinstalls them.
        self.class_atoms = UNRESOLVED_CLASS_ATOMS;
        // Likewise the literal area: it belongs to the module being run and is
        // re-bound by `run_loop`.
        self.literals = None;
    }

    /// Bind the literal area of the module about to be executed.
    ///
    /// Called once per run by the dispatch loop ([`crate::interpreter`]) with the
    /// `Arc` cloned out of the `&Module` being executed; see the field docs for
    /// why the binding is sound for the whole run.
    pub(crate) fn set_literals(&mut self, literals: SharedLiteralArea) {
        self.literals = Some(literals);
    }

    /// The static literal at `index` of the module currently being executed (an
    /// `Operand::Literal(index)` reference).
    ///
    /// `None` when no module is bound (outside a run) or the index is outside the
    /// module's `LitT` table. Delegates to
    /// [`beam_loader::LiteralArea::get`] — the single source of truth for a
    /// literal index.
    pub fn literal(&self, index: u32) -> Option<Eterm> {
        self.literals.as_ref()?.get(index)
    }

    /// Allocate an **off-heap ref-counted binary payload** holding `bytes` and
    /// return a pointer to its first data byte.
    ///
    /// This is the payload half of the frozen refc-binary representation
    /// ([`beam_term::refc`]): the returned pointer is exactly the value the
    /// `BinRef.val` word of an `[BinRef][ErlSubBits]` term must hold, and it stays
    /// valid (and unmoved) until this machine is dropped — the machine keeps the
    /// [`beam_alloc::RefcBinary`] handle that owns the payload in
    /// [`Machine::refc_binaries`], which [`Machine::reset`] never clears.
    ///
    /// The payload is `beam_alloc`'s `binary_alloc` allocation and is *immutable*:
    /// there is no in-place growth, so a caller that needs a larger binary builds a
    /// new one (mirroring C's copy fallback in `erts_bs_private_append_checked`).
    pub(crate) fn alloc_refc_binary(&mut self, bytes: &[u8]) -> Result<*mut u8> {
        let binary = beam_alloc::RefcBinary::new(bytes)?;
        let data = binary.data_ptr();
        self.refc_binaries.push(binary);
        Ok(data)
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

    /// `trim N` — shrink the current frame to its **highest** Y slots.
    ///
    /// Reproduces C's `i_trim` (`../../erts/emulator/beam/emu/instrs.tab:593`):
    /// `E += N`, which moves the frame's base *up* by `N` and therefore
    /// **discards the lowest `N` Y slots**, renumbering the survivors
    /// (`y(k) → y(k - N)`). The compiler agrees: `beam_trim.erl` treats
    /// `trim N` as eliminating `y(0) .. y(N-1)` and remaps the rest by `-N`
    /// (`Eliminated = [{y,N} || N <- seq(0, Trim-1)]`).
    ///
    /// The frame keeps `y_count - N` slots and its `y_base` is unchanged; only the
    /// innermost frame's slots (the tail of the stack) are affected.
    pub(crate) fn trim(&mut self, n: u32) -> Result<()> {
        let n_usize = n as usize;
        let (y_base, y_count) = match self.frames.last() {
            Some(frame) => (frame.y_base, frame.y_count),
            None => return Err(error::unsupported("trim: no frame")),
        };
        if n > y_count {
            return Err(error::unsupported("trim: n exceeds frame size"));
        }
        let keep = y_count as usize - n_usize;
        if n_usize > self.stack.len() {
            return Err(error::unsupported("trim: stack underflow"));
        }
        // Drop the lowest `n` Y slots and shift the survivors down so `y(k)`
        // addresses `y(k + n)` of the pre-trim frame.
        if n_usize > 0 {
            self.stack
                .copy_within(y_base + n_usize..y_base + y_count as usize, y_base);
        }
        self.stack.truncate(y_base + keep);
        if let Some(frame) = self.frames.last_mut() {
            frame.y_count = keep as u32;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Exception scaffolding (the shared state the `families::exceptions` bodies use)
    // -----------------------------------------------------------------------

    /// The `(function index, code offset)` of every live call frame that is
    /// currently **suspended in a call**, innermost frame first.
    ///
    /// A frame's continuation-pointer slot holds the address its function resumes
    /// at once the call it is waiting for returns ([`Instr::Call`](beam_loader::Instr)
    /// writes it through [`Machine::set_continuation`]), so this sequence is the
    /// engine's replacement for the C runtime's **saved raw stacktrace**: the chain
    /// of callers, innermost first — the same order C's `build_stacktrace` emits
    /// (`../../erts/emulator/beam/beam_common.c:1274-1288`).
    ///
    /// A frame sitting at [`Continuation::Halt`] contributes nothing: that is the
    /// bottom frame (pushed by [`Machine::run_label`]) and any frame whose function
    /// has **not** yet made a call — in particular the frame of the function that
    /// is executing right now, whose own location the dispatch loop already knows
    /// and supplies itself (see `crate::families::exceptions`).
    ///
    /// Read-only introspection (no state is changed); the caller is the
    /// Milestone-1 stacktrace builder.
    pub fn resumption_addresses(&self) -> Vec<(u32, u32)> {
        self.frames
            .iter()
            .rev()
            .filter_map(|frame| match frame.continuation {
                Continuation::Address { func, pc } => Some((func, pc)),
                Continuation::Halt => None,
            })
            .collect()
    }

    /// Push a catch frame (the `try`/`catch` handler target).
    pub fn push_catch_frame(&mut self, frame: CatchFrame) {
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
        Ok(Step::Goto {
            func: frame.func,
            pc: frame.pc,
        })
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
const UNRESOLVED_CLASS_ATOMS: [Eterm; 3] = [THE_NON_VALUE; 3];

impl Default for Machine {
    /// Same as [`Machine::new`]: the default register file ([`MAX_REGISTER`]).
    fn default() -> Machine {
        Machine::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Mailbox, MinimalContext};
    use beam_loader::{Export, Function, Instr, Label, Operand};
    use beam_term::{make_atom, make_small};
    use std::collections::VecDeque;

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
    fn trim_discards_the_lowest_y_slots_and_renumbers() {
        // C's `i_trim` does `E += N`: the frame base moves *up*, so the lowest
        // `N` Y slots are discarded and the survivors are renumbered down
        // (`y(k) -> y(k - N)`). `beam_trim.erl` eliminates `y(0)..y(N-1)`.
        let mut machine = Machine::new();
        machine.push_frame(4).unwrap();
        machine.set_y(0, Eterm::from_raw(1)).unwrap();
        machine.set_y(2, Eterm::from_raw(3)).unwrap();
        machine.set_y(3, Eterm::from_raw(4)).unwrap();
        machine.trim(2).unwrap();
        assert_eq!(machine.stack_depth(), 2);
        // Old y2 (`3`) is now y0; old y3 (`4`) is now y1; old y0/y1 are gone.
        assert_eq!(machine.y(0), Some(Eterm::from_raw(3)));
        assert_eq!(machine.y(1), Some(Eterm::from_raw(4)));
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

    #[test]
    fn resumption_addresses_walk_the_callers_innermost_first() {
        let mut machine = Machine::new();
        // A frame-less machine has nothing suspended in a call.
        assert!(machine.resumption_addresses().is_empty());

        // The bottom frame halts, so it contributes nothing.
        machine.push_frame(0).unwrap();
        assert!(machine.resumption_addresses().is_empty());

        // F (frame 1) called G: F resumes at (7, 3) once G returns.
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(Continuation::Address { func: 7, pc: 3 })
            .unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(7, 3)]);

        // G (frame 2) — the frame of the function executing right now — has not
        // made a call yet, so it contributes nothing and the caller stays first.
        machine.push_frame(1).unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(7, 3)]);

        // G then called H: the caller chain is now H's caller (G at (9, 1)) then
        // G's caller (F at (7, 3)) — innermost first.
        machine
            .set_continuation(Continuation::Address { func: 9, pc: 1 })
            .unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(9, 1), (7, 3)]);

        // Popping the innermost frame drops its entry.
        machine.pop_frame().unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(7, 3)]);
    }

    // ---- the blocking run outcome -----------------------------------------

    /// A one-function module: entry label `Label(2)` (immediately after
    /// `Label(1)`), body `code`, exported as `name/0`.
    fn module_with_code(name: &str, code: Vec<Instr>) -> (Module, Atom) {
        let mut module = Module::default();
        let function_name = module.atom_table.intern(name);
        let labels = code
            .iter()
            .enumerate()
            .filter_map(|(i, ins)| match ins {
                Instr::Label(l) => Some((*l, i)),
                _ => None,
            })
            .collect();
        module.functions = vec![Function {
            func_label: Label(1),
            entry_label: Label(2),
            module: Atom::MAX,
            name: function_name,
            arity: 0,
            code,
            blocks: Vec::new(),
            labels,
        }];
        module.exports = vec![Export {
            function: function_name,
            arity: 0,
            label: Label(2),
        }];
        (module, function_name)
    }

    /// A mailbox that hands out its queue one message per `receive_next`, and
    /// ignores `to` in `send_message` (enough to script a receive).
    struct ScriptedMailbox {
        messages: VecDeque<Eterm>,
    }

    impl Mailbox for ScriptedMailbox {
        fn send_message(&mut self, _to: Eterm, message: Eterm) -> Result<()> {
            self.messages.push_back(message);
            Ok(())
        }

        fn receive_next(&mut self) -> Result<Option<Eterm>> {
            Ok(self.messages.pop_front())
        }

        fn receive_skip(&mut self) -> Result<()> {
            Ok(())
        }

        fn receive_remove(&mut self) -> Result<()> {
            Ok(())
        }

        fn receive_reset(&mut self) -> Result<()> {
            Ok(())
        }

        fn receive_marker_reserve(&mut self) -> Result<Eterm> {
            Ok(NIL)
        }

        fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
            Ok(())
        }

        fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
            Ok(())
        }

        fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
            Ok(())
        }
    }

    /// The receive sequence the compiler emits, hand-written:
    ///
    /// ```text
    ///  1: label 2                 ; entry
    ///  2: loop_rec {f,4} x(0)     ; take the next candidate, else jump to the wait
    ///  3: remove_message          ; the (unconditional) match succeeded
    ///  4: jump {f,5}              ; leave the receive
    ///  5: label 4
    ///  6: wait {f,3}              ; mailbox empty — suspend, resuming at label 3
    ///  7: label 3
    ///  8: jump {f,2}              ; re-examine the mailbox
    ///  9: label 5
    /// 10: return
    /// ```
    fn receive_module() -> (Module, Atom) {
        module_with_code(
            "recv",
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::LoopRec {
                    fail: Label(4),
                    dst: Operand::X(0),
                },
                Instr::RemoveMessage,
                Instr::Jump { target: Label(5) },
                Instr::Label(Label(4)),
                Instr::Wait { target: Label(3) },
                Instr::Label(Label(3)),
                Instr::Jump { target: Label(2) },
                Instr::Label(Label(5)),
                Instr::Return,
            ],
        )
    }

    #[test]
    fn run_export_outcome_reports_a_returned_value() {
        let (module, name) = module_with_code(
            "f",
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Move {
                    src: Operand::Integer(5),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert_eq!(
            machine
                .run_export_outcome(&module, name, 0, &[], &mut ctx)
                .unwrap(),
            RunOutcome::Returned(make_small(5))
        );
    }

    #[test]
    fn a_receive_on_an_empty_mailbox_blocks_then_resumes_after_a_message_arrives() {
        let (module, name) = receive_module();
        let mut machine = Machine::new();
        // An installed but empty mailbox: exhaustion is an `Ok(None)` answer from
        // the host, not an error.
        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::new(),
        }));

        // Empty mailbox: `loop_rec` jumps to the `wait`, which suspends. The
        // reported address is the `wait`'s target (`label 3`, code offset 7) — the
        // `loop_rec` that re-examines the mailbox once a message arrives.
        assert_eq!(
            machine
                .run_export_outcome(&module, name, 0, &[], &mut ctx)
                .unwrap(),
            RunOutcome::Wait { func: 0, pc: 7 }
        );

        // The suspension left the call frames in place, so the process can be
        // resumed in the *same* machine; a non-blocking entry would instead report
        // the typed block.
        assert_eq!(machine.frame_depth(), 1, "the bottom frame survived");

        // A message arrives, then the host resumes at the reported address.
        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(42)]),
        }));
        assert_eq!(
            machine.resume_outcome(&module, 0, 7, &mut ctx).unwrap(),
            RunOutcome::Returned(make_small(42))
        );
    }

    #[test]
    fn run_export_folds_a_blocked_receive_into_the_typed_blocked_error() {
        let (module, name) = receive_module();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::new(),
        }));

        let err = machine
            .run_export(&module, name, 0, &[], &mut ctx)
            .unwrap_err();

        match err {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: run: process blocked (no message available)")
            ),
            other => panic!("expected the typed blocked error, got {other:?}"),
        }
        // `error::blocked()` is the single definition of that reason string.
        match error::blocked() {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: run: process blocked (no message available)")
            ),
            other => panic!("expected the typed blocked error, got {other:?}"),
        }
    }

    #[test]
    fn resume_outcome_does_not_reset_the_suspended_machine() {
        // A resumed machine keeps the x-registers written before the suspension:
        // `resume_outcome` must not reset (a reset would also drop the bottom frame
        // the trailing `return` needs).
        let (module, name) = receive_module();
        let mut machine = Machine::new();
        let mut empty = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::new(),
        }));
        assert_eq!(
            machine
                .run_export_outcome(&module, name, 0, &[], &mut empty)
                .unwrap(),
            RunOutcome::Wait { func: 0, pc: 7 }
        );
        // Sentinel in a register the resumed code never writes.
        machine.set_x(9, make_small(0x5A)).unwrap();

        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(7)]),
        }));
        assert_eq!(
            machine.resume_outcome(&module, 0, 7, &mut ctx).unwrap(),
            RunOutcome::Returned(make_small(7))
        );
        assert_eq!(machine.x(9), Some(make_small(0x5A)));
    }
}
