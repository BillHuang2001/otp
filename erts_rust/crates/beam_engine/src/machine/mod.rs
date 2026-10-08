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
//!
//! ## Submodules
//!
//! The machine core is split by responsibility; this module owns the [`Machine`]
//! state itself (its fields, construction and the module-owned literal area) and
//! re-exports every name the rest of the crate reaches as `crate::machine::…`, so
//! the split is invisible to callers:
//!
//! * `registers` — the x-register and float-register (FR) files, their bounds
//!   ([`MAX_REGISTER`]/[`MAX_FLOAT_REGISTER`]) and [`Machine::reset`].
//! * `frames` — [`Continuation`]/[`Frame`], the Y-slot and call-frame primitives,
//!   the heap request seam ([`Machine::ensure_heap`]) and the caller-chain
//!   introspection ([`Machine::resumption_addresses`]).
//! * `catch` — [`CatchFrame`] and the exception scaffolding: the catch-frame stack
//!   plus the one uniform raise/unwind entry points ([`Machine::throw`]/
//!   [`Machine::throw_with_trace`]) and the class-atom installation.
//! * `run` — the run entries ([`Machine::run_export`]/[`Machine::run_label`]/
//!   [`Machine::run_with_tier`]), their blocking-aware `*_outcome` counterparts and
//!   [`RunOutcome`].
//! * `location` — the read-only execution-position introspection
//!   ([`Machine::current_code`]/[`Machine::entry_call`] and the derived
//!   [`Machine::current_function`]/[`Machine::initial_call`]/
//!   [`Machine::current_location`]/[`Machine::current_stacktrace`]).
//! * `tests` — this module's unit tests, which exercise every part together.

use beam_common::{ExceptionClass, Result};
use beam_loader::SharedLiteralArea;
use beam_term::Eterm;

pub use self::catch::CatchFrame;
pub(crate) use self::frames::{Continuation, Frame};
pub use self::registers::{MAX_FLOAT_REGISTER, MAX_REGISTER};
pub use self::run::RunOutcome;

use self::catch::UNRESOLVED_CLASS_ATOMS;

mod catch;
mod frames;
mod location;
mod registers;
mod run;
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
    /// [`Module`](beam_loader::Module) before raising ([`Machine::set_class_atom`])
    /// and the raise fails with a typed error when the class atom is missing
    /// (never a wrong atom).
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
    /// loop through `CallExt` into the host [`ExecutionContext`](crate::context::ExecutionContext),
    /// which never re-enters the engine with a different `&Module` on the same
    /// run. A fresh `run_*` calls [`Machine::reset`] (clearing this) and re-binds
    /// it, so the area always matches the module being interpreted.
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
    /// The code address the machine is currently executing, or was most recently
    /// **suspended** at: `(function index, code offset)`.
    ///
    /// Updated by [`Machine::step_instruction`](crate::machine::Machine) — the
    /// single choke point every *interpreted* instruction passes through (the
    /// interpreter's dispatch loop and the native tiers' per-instruction deopt
    /// helper both go through it) — and cleared by
    /// [`Machine::reset`](crate::machine::Machine::reset). `None` before the
    /// machine has interpreted anything.
    ///
    /// This is the engine's read-only "saved instruction pointer": the input to
    /// the execution-position introspection ([`Machine::current_code`], and the
    /// derived [`Machine::current_function`]/[`Machine::current_location`]/
    /// [`Machine::current_stacktrace`]). Every **blocking** instruction
    /// (`wait`/`wait_timeout`) is interpreted rather than inlined by a native
    /// tier, so the recorded address is exact at every suspension point; a
    /// natively-executed JIT block does not update it, so mid-block it is the
    /// *last interpreted* instruction (documented, never invented).
    pub(crate) current_code: Option<(u32, u32)>,
    /// The `(function index, arity)` this run was **entered** at — the engine's
    /// analogue of ERTS's `$initial_call`.
    ///
    /// Recorded once per run by [`Machine::run_loop`](crate::machine::Machine)
    /// (the first function entered after a [`Machine::reset`](crate::machine::Machine::reset),
    /// which clears it) and read back by [`Machine::initial_call`]. `None` before
    /// any run.
    pub(crate) entry_call: Option<(u32, u32)>,
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
            current_code: None,
            entry_call: None,
        };
        machine.reset();
        machine
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
    #[inline]
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
}

impl Default for Machine {
    /// Same as [`Machine::new`]: the default register file ([`MAX_REGISTER`]).
    fn default() -> Machine {
        Machine::new()
    }
}
