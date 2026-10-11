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
//! * `context` — the **module context** registry ([`ModuleContext`]/[`Contexts`]):
//!   the callee modules a `call_ext*` entered in *this* machine.
//! * `drive` — the module-context-aware dispatch driver
//!   ([`Machine::drive`]), shared by the interpreter entries and a resume.
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
//!   [`Machine::current_location`]/[`Machine::current_stacktrace`]), plus the
//!   parked-`receive` deadline seam
//!   ([`Machine::wait_deadline`]/[`Machine::timeout_delay`] and [`WaitDeadline`]).
//! * `tests` — the cross-cutting end-to-end tests (the uniform raise funnel's
//!   symbolic frames, run through real run entries), over the synthetic-module
//!   scaffolding in `test_support`.
//! * `deadline_tests` — the parked-`receive` deadline driven to completion on a real
//!   compiled module (`erlc`) and cross-checked against the system `erl`, including the
//!   nested / blocking run path a fired `after T` has to work through.
//! * `test_support` — the `cfg(test)`-only synthetic-module builder (`Fixture`)
//!   and the cross-module seam context (`seam`) the `machine` tests share.
use beam_common::{ExceptionClass, Result};
use beam_loader::{Module, SharedLiteralArea};
use beam_term::Eterm;

pub use self::catch::CatchFrame;
pub(crate) use self::context::{Contexts, ModuleContext};
pub(crate) use self::frames::{Continuation, Frame};
pub use self::location::WaitDeadline;
pub use self::registers::{MAX_FLOAT_REGISTER, MAX_REGISTER};
pub use self::run::RunOutcome;

use crate::context::{ExecutionContext, RootSet};

use self::catch::UNRESOLVED_CLASS_ATOMS;

mod catch;
mod context;
#[cfg(test)]
mod deadline_tests;
mod drive;
mod frames;
mod location;
mod registers;
mod run;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod test_support;/// Process-state core: x-register file, Erlang stack and call-frame stack.
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
    /// The static-literal area of the module currently being executed.
    ///
    /// Bound at the start of every run ([`Machine::drive`] clones the `Arc` out of
    /// the module context being entered) and cleared by [`Machine::reset`]. It lets
    /// the shared operand seam ([`crate::operand::read_operand`]) resolve
    /// `Operand::Literal(idx)` through [`Machine::literal`] without threading
    /// `&Module` through every call site.
    ///
    /// **The binding follows the module context.** A `call_ext*` the host resolved
    /// through
    /// [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)
    /// runs the callee *in this machine*, so the executing module changes
    /// mid-run: [`Machine::switch_context`] re-binds this field to the entered
    /// context's area (and back to [`Machine::entry_literals`] on the way out), so
    /// the area always matches the module whose instructions are being interpreted.
    ///
    /// A literal [`Eterm`] points into the area's **own** heap (never a process
    /// heap). At Milestone 1 there is no GC, so reading/comparing/copying a
    /// literal is safe; once the Stage-4 collector lands a literal must be treated
    /// as permanent (never scanned/copied as a process-heap term).
    literals: Option<SharedLiteralArea>,
    /// The literal area of the **entry** module (module context `0`).
    ///
    /// Context `0` is the module the run was entered in, which the machine never
    /// owns (the drive loop borrows it), so its literal area cannot be recovered
    /// from [`Machine::contexts`] the way a registered context's can. Recording it
    /// here is what lets [`Machine::switch_context`] restore context `0`'s binding
    /// when a cross-module callee returns into the caller. Set by
    /// [`Machine::enter_run`], cleared by [`Machine::reset`].
    pub(crate) entry_literals: Option<SharedLiteralArea>,
    /// The registered module contexts (`> 0`) entered through the cross-module
    /// call seam — see [`ModuleContext`] / [`Contexts`].
    ///
    /// Context `0` is implicit (the drive loop's entry module) and has no entry
    /// here. **Cleared by [`Machine::reset`] but not by a resume**: a suspended
    /// cross-module frame must still be able to name its module when the host
    /// resumes the process ([`Machine::resume_outcome`]).
    pub(crate) contexts: Contexts,
    /// The module context currently executing: `0` for the run's entry module,
    /// otherwise a [`Contexts`] id.
    ///
    /// The driver keeps it in step with [`Machine::literals`] (via
    /// [`Machine::switch_context`]) and a suspension leaves it at the context the
    /// process blocked in, which is how a resume recovers the module identity of
    /// the reported `(func, pc)`.
    pub(crate) context: u32,
    /// The **raise-time snapshot** of the pre-unwind frame chain: the engine's
    /// replacement for C's saved raw `StackTrace` (`c_p->ftrace`).
    ///
    /// Each entry is `(context, func, pc)`: the **raising** frame first, then every
    /// live caller innermost-first — the order C's `build_stacktrace` emits. It is
    /// rebuilt by [`Machine::throw_with_trace`] **before** it unwinds the
    /// call-frame stack, so the handler's `build_stacktrace` (and the uncaught
    /// funnel [`Machine::with_uncaught_frames`]) can still name the frames the
    /// raise unwound out of. Read back through [`Machine::raise_snapshot`].
    ///
    /// Cleared by [`Machine::reset`] (the snapshot belongs to the run that is
    /// starting) but **not** by a resume ([`Machine::resume_outcome`]), which must
    /// preserve it along with the machine's other suspended state.
    pub(crate) raise_snapshot: Vec<(u32, u32, u32)>,
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
    /// A **reusable argument scratch buffer** for the rare wide BIF call.
    ///
    /// The interpreter marshals the arguments of a `gc_bif`/`bif`/`bif0`/
    /// `call_ext*` through a fixed stack array for every arity up to its
    /// `STACK_ARGS` arity bound — the common case, which therefore allocates
    /// nothing. A **wider** call (a `call_ext` to a many-argument function) reuses
    /// this buffer instead of minting a fresh `Vec` per call: the capacity is
    /// retained across calls, so the steady state allocates nothing either.
    ///
    /// The buffer is always **left empty** by the dispatcher (only its capacity
    /// survives), so it never keeps a term alive and [`Machine::reset`] has nothing
    /// to clear.
    pub(crate) args_scratch: Vec<Eterm>,
    /// The **per-run tracing gate** (contract A.8): the cached answer of
    /// [`ExecutionContext::trace_enabled`](crate::context::ExecutionContext::trace_enabled)
    /// for the run currently executing.
    ///
    /// The dispatch loop's call/return arms consult this plain `bool` (one
    /// predictable load) instead of making a `&mut dyn ExecutionContext` vtable call
    /// **per call/return instruction**; the single `trace_enabled()` call happens
    /// **once per run**, at the driver entry (`Machine::run_loop_with_map` on the
    /// interpreter, the shared native driver on the baseline/Cranelift tiers).
    ///
    /// **Semantics.** The flag is latched at a run/resume entry, so a host that
    /// turns tracing on or off **mid-run** sees the change take effect at the
    /// process's *next* run entry or [`Machine::resume_outcome`] — not at the next
    /// instruction. This is exactly the granularity a host needs (its
    /// `trace_enabled()` is consulted once per run, not per instruction) and it keeps
    /// the per-instruction hot path to a single `bool` load.
    ///
    /// [`Machine::reset`](crate::machine::Machine::reset) leaves it `false`, so a run
    /// that never sets it (e.g. a direct `Machine::step_instruction` call in a test)
    /// cannot leak a stale `true` from an earlier run.
    pub(crate) trace_on: bool,
    /// The **per-run reduction-hook gate**: the cached answer of
    /// [`ExecutionContext::wants_reduce`](crate::context::ExecutionContext::wants_reduce)
    /// for the run currently executing.
    ///
    /// [`Machine::drive`](crate::machine::Machine) charges one reduction per
    /// interpreted instruction and offers each charge to the host through
    /// [`ExecutionContext::reduce`](crate::context::ExecutionContext::reduce) — a
    /// `&mut dyn ExecutionContext` vtable crossing the interpreter cannot elide. When
    /// the host's [`wants_reduce`](crate::context::ExecutionContext::wants_reduce) is
    /// `false` (its `reduce` is provably the default no-op), the driver skips that
    /// per-instruction call so the hot path pays a single `bool` load instead.
    ///
    /// The default is **conservative `true`**: a host that pre-empts by returning an
    /// error from `reduce` is never silently skipped. The slice counter
    /// ([`Machine::reductions`](crate::machine::Machine::reductions)) is bumped on
    /// every interpreted instruction regardless of this flag, so opting out changes
    /// **only** whether the host hook is called, never the observable count.
    ///
    /// Latched at a run/resume entry exactly like [`Machine::trace_on`], and
    /// [`Machine::reset`](crate::machine::Machine::reset) leaves it `false`, so a run
    /// that never latches it (e.g. a direct `Machine::step_instruction` call in a
    /// test) cannot leak a stale value from an earlier run.
    pub(crate) reduce_on: bool,
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
    /// The number of **interpreted instructions charged in the current scheduling
    /// slice** — the engine half of the runtime's *reduction* accounting.
    ///
    /// Zeroed at every [`Machine::drive`](crate::machine::Machine) entry (one `drive`
    /// call is one run or one resume, i.e. one slice) and incremented once per
    /// `step_instruction`, so [`Machine::reductions`](crate::machine::Machine) read
    /// when the slice ends is that slice's instruction count.
    ///
    /// The engine's other charging point is
    /// [`ensure_heap`](crate::ExecutionContext::ensure_heap) (reached by
    /// `test_heap`/`allocate_heap`), which prices *heap admissions* — a tight
    /// **non-allocating** loop never reaches it, which is why the per-instruction
    /// charge exists. A host observes the charge **during** the slice through
    /// [`reduce`](crate::ExecutionContext::reduce) and may pre-empt there; it never
    /// has to poll this field mid-run.
    ///
    /// Only the **interpreter's** dispatch loop
    /// ([`Machine::drive`](crate::machine::Machine)) charges: a natively executed JIT
    /// block bypasses `step_instruction`, so this is a lower bound under the baseline
    /// tier (documented, never a fabricated count).
    pub(crate) reductions: u64,
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
            entry_literals: None,
            contexts: Contexts::new(),
            context: 0,
            raise_snapshot: Vec::new(),
            refc_binaries: Vec::new(),
            args_scratch: Vec::new(),
            trace_on: false,
            reduce_on: false,
            current_code: None,
            entry_call: None,
            reductions: 0,
        };
        machine.reset();
        machine
    }

    /// Bind the literal area of the module context about to be executed.
    ///
    /// Called by the dispatch loop ([`crate::interpreter`]) with the `Arc` cloned
    /// out of the module being executed — the entry module at a run entry, and an
    /// entered callee's module at a context switch; see the field docs for why the
    /// binding is sound for as long as that context executes.
    pub(crate) fn set_literals(&mut self, literals: SharedLiteralArea) {
        self.literals = Some(literals);
    }

    /// The module context currently executing (`0` = the run's entry module).
    pub(crate) fn current_context(&self) -> u32 {
        self.context
    }

    /// The whole Erlang stack (every Y slot of every live frame) as a mutable slice
    /// — part of the **root-set hand-off**.
    ///
    /// See [`Machine::x_mut`] for why the whole-set hand-off goes through
    /// [`Machine::with_roots`] rather than three separate accessor calls.
    #[inline]
    pub fn stack_mut(&mut self) -> &mut [Eterm] {
        &mut self.stack
    }

    /// Run `f` with the engine's **whole live root set** handed over as a
    /// [`RootSet`]: the X-register file, the whole Erlang stack and the whole FR
    /// file, all borrowed mutably and disjointly from one `&mut self`.
    ///
    /// This is the engine's root hand-off for an allocation that may collect
    /// ([`Machine::alloc_rooted`] is the convenience wrapper). The three files are
    /// borrowed **together** — three separate `x_mut`/`stack_mut`/`fr_mut` calls
    /// cannot coexist, so a caller needing more than one of them must come through
    /// here. `live` declares how many leading X registers are live for the
    /// operation in flight; [`RootSet::ALL_LIVE`] is the conservative choice (a
    /// copying collector may always over-approximate the root set — it can only
    /// keep a term alive, never free a live one). The root-set contract itself is
    /// the frozen one on [`RootSet`].
    pub fn with_roots<R>(&mut self, live: u32, f: impl FnOnce(RootSet<'_>) -> R) -> R {
        let roots = RootSet::new(&mut self.x, &mut self.stack, &mut self.fr, live);
        f(roots)
    }

    /// Allocate `n` heap words **with the live root set handed to `ctx`**, so a
    /// context with a real collector can collect (and move terms) as part of the
    /// allocation.
    ///
    /// This is the engine-side twin of
    /// [`ExecutionContext::alloc_words`](crate::context::ExecutionContext::alloc_words):
    /// where `alloc_words` gives the host no admission point and no roots,
    /// `alloc_rooted` hands over the machine's whole root set (see
    /// [`Machine::with_roots`]) and calls
    /// [`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted).
    /// The default `alloc_words_rooted` discards the roots and forwards to
    /// `alloc_words`, so a host without a collector behaves exactly as before; a host
    /// with one runs the collector over `roots` first, writing any moved term back
    /// through the mutable slices, and may then hand back a different (moved) heap.
    ///
    /// `live` declares how many leading X registers are live (use
    /// [`RootSet::ALL_LIVE`](crate::context::RootSet::ALL_LIVE) when unknown). The
    /// returned pointer is uninitialized storage valid **until the next
    /// `ensure_heap`/collecting allocation**, exactly as `alloc_words` documents —
    /// so the caller must re-read any live term from `x`/`stack`/`fr` afterwards.
    pub fn alloc_rooted(
        &mut self,
        ctx: &mut dyn ExecutionContext,
        n: usize,
        live: u32,
    ) -> Result<*mut Eterm> {
        self.with_roots(live, |roots| ctx.alloc_words_rooted(n, roots))
    }
    /// Register `module` as an enterable module context and return its id (`>= 1`).
    ///
    /// Idempotent by the `Arc`'s allocation identity (see [`Contexts::register`]),
    /// so a repeated or recursive cross-module call reuses one context.
    pub(crate) fn register_context(&mut self, module: &std::sync::Arc<Module>) -> u32 {
        self.contexts.register(module)
    }

    /// Land the machine in module context `id`, re-binding [`Machine::literals`].
    ///
    /// A no-op when `id` is already the current context (so the hot path pays one
    /// integer compare). Otherwise the literal area of the context's module is
    /// bound: context `0` restores [`Machine::entry_literals`], a registered
    /// context its own module's area. An unregistered id (or a context `0` whose
    /// entry area was never bound) is the typed
    /// [`unsupported`](crate::error::unsupported) error — never a stale area, which
    /// would resolve an `Operand::Literal` against the wrong module.
    pub(crate) fn switch_context(&mut self, id: u32) -> Result<()> {
        if id == self.context {
            return Ok(());
        }
        let literals = match id {
            0 => self.entry_literals.clone(),
            _ => self.contexts.handle(id).map(|entry| entry.module.literals.clone()),
        };
        let Some(literals) = literals else {
            return Err(crate::error::unsupported("run: unknown module context"));
        };
        self.set_literals(literals);
        self.context = id;
        Ok(())
    }

    /// The handle of registered module context `id`, or `None` for the entry
    /// context `0` / an id never registered.
    pub(crate) fn module_context(&self, id: u32) -> Option<std::sync::Arc<ModuleContext>> {
        self.contexts.handle(id)
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

    /// The **off-heap ref-counted binary payload handles** this machine keeps alive
    /// for the refc terms it built: the read-only view of
    /// [`Machine::refc_binaries`], in build order.
    ///
    /// A `bs_init_writable` / `bs_create_bin` result larger than
    /// [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`] bytes is stored off the process
    /// heap: the on-heap term is the frozen 8-word `[BinRef][ErlSubBits]` pair whose
    /// `BinRef.val` word points at the payload ([`beam_term::refc`]), and the
    /// payload itself is one of these [`beam_alloc::RefcBinary`] handles
    /// (`Machine::alloc_refc_binary`).
    ///
    /// **Why a host reads them.** A host that moves a term off this machine — a
    /// message send, a process return — copies it with the copy engine, which
    /// resolves a refc binary's payload **by its data pointer** through the copy's
    /// *source* off-heap table (`beam_message::OffHeapTable::binary_by_data_ptr`).
    /// The empty source table the simple copy path (`copy_struct_simple`) uses
    /// cannot serve that lookup, so the host first **clones** the handles the copy
    /// needs into that source table (`beam_message::OffHeapTable::push_binary`).
    /// Cloning a handle bumps the [`beam_alloc::RefcBinary`] refcount, so a cloned
    /// handle kept in a destination table keeps the payload alive (and unmoved)
    /// after this machine is dropped.
    ///
    /// The slice is **never cleared by [`Machine::reset`]**, so a payload a returned
    /// term still points at survives the next run.
    pub fn refc_binaries(&self) -> &[beam_alloc::RefcBinary] {
        &self.refc_binaries
    }
}

impl Default for Machine {
    /// Same as [`Machine::new`]: the default register file ([`MAX_REGISTER`]).
    fn default() -> Machine {
        Machine::new()
    }
}
