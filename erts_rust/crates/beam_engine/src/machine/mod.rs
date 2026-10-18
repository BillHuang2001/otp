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
use beam_common::{ExceptionClass, Result, StackTrace};
use beam_loader::{Module, SharedLiteralArea};
use beam_term::Eterm;

pub use self::catch::CatchFrame;
pub(crate) use self::catch::namespace_atom;pub(crate) use self::context::{Contexts, ModuleContext};
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
    /// The **written** high-water mark (exclusive) of the x-register file: one past the
    /// highest slot a register-write seam has stored into since the last refill
    /// ([`Machine::reset`]).
    ///
    /// Every x-register write the engine performs funnels through [`Machine::set_x`]
    /// (the `move`/`get_*`/`gc_bif` destination seam and the argument seeding at a run
    /// entry), which raises this mark. It exists so [`Machine::reset`] can refill only
    /// the slots that may hold a term and skip the rest of the 1024-word file, which is
    /// already `NIL` — a full refill of both files is otherwise the whole per-run floor.
    ///
    /// **It is a conservative approximation, never a guarantee.** A writer that can
    /// reach the register file without going through `set_x` cannot report what it
    /// touched: a native JIT block stores straight into `machine.x` through the raw
    /// pointer the generated code holds, and a host handed a raw `&mut` slice
    /// ([`Machine::x_mut`]/[`Machine::with_roots`]) may write any slot. Each such writer
    /// sets [`Machine::regs_escaped`], which forces the next `reset` to refill the whole
    /// file. A stale register is a WRONG RESULT, so when in doubt the reset refills in
    /// full.
    pub(crate) x_mark: u32,
    /// The FR-file twin of [`Machine::x_mark`]: raised by [`Machine::set_fr`], forced
    /// full by [`Machine::regs_escaped`].
    pub(crate) fr_mark: u32,
    /// Whether a writer that cannot report the register slots it touched has run since
    /// the last refill, so [`Machine::x_mark`]/[`Machine::fr_mark`] understate the dirty
    /// region.
    ///
    /// Set by [`Machine::x_mut`]/[`Machine::fr_mut`]/[`Machine::with_roots`] (the caller
    /// may write any slot of the slice it is handed) and by the native driver before it
    /// folds a compiled block's result ([`Machine::mark_regs_escaped`]). [`Machine::reset`]
    /// refills the whole register files when it is set and then clears it.
    pub(crate) regs_escaped: bool,
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
    /// Whether [`Machine::ensure_class_atoms`] has already resolved the three
    /// exception-class atoms from the engine's live atom namespace for the run in
    /// progress.
    ///
    /// A per-run latch: the namespace cannot change mid-run, so the whole resolution
    /// runs at most once per run and the single caller
    /// ([`crate::families::dispatch`]) pays one `bool` test in steady state. Cleared
    /// by [`Machine::reset`] — the class atoms belong to the run that is starting.
    pub(crate) class_atoms_live: bool,    /// The register-file size (a configurable bound, default [`MAX_REGISTER`]).
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
    /// A **one-shot latch**: `true` means "the next raise that actually lands was
    /// raised by a call site that knows its unresolvable callee and has published
    /// that callee's `{Module,Function,ArgList,[]}` item as the raise's raw trace".
    ///
    /// Set by the failure sites that synthesize such an item (a `call_ext*` import
    /// or an `apply(M,F,A)` target the host cannot resolve — see
    /// [`crate::families::exceptions::callee`]) **immediately before** the
    /// `throw_with_trace` that lands the raise, and consumed by that call (see
    /// [`Machine::raise_head_item`]). It is a separate latch from the live-raise
    /// state so a propelling `Err` path — a raise with no live catch frame, which
    /// never reaches `throw_with_trace` — cannot leave a stale head for a later,
    /// unrelated raise to inherit.
    pub(crate) pending_callee_head: bool,
    /// The **live raise** state behind [`Machine::pending_callee_head`]: `true`
    /// while the raise currently in flight was raised with a synthesized
    /// `{Module,Function,ArgList,[]}` head item as its raw trace.
    ///
    /// [`Machine::throw_with_trace`] sets it (as its first act, from the one-shot
    /// latch), `build_stacktrace` consumes it (the cooked trace prepends the head
    /// item — see [`crate::families::exceptions::stacktrace`]) and so a raise can
    /// never inherit a stale head from an earlier one. Both flags are metadata
    /// (`bool`) only: the head item itself is the raise's raw-trace **word**,
    /// published in `x(2)`/`x(0)`, never a machine-held heap term — which is what
    /// keeps it inside the frozen root set ([`crate::context::RootSet`]) across the
    /// rooted reservation the cooked trace takes.
    pub(crate) raise_head_item: bool,
    /// A **one-shot latch**: the argument list of the function whose clause failed.
    ///
    /// Set by `crate::interpreter::dispatch::raise_function_clause_with_args` — the ONE
    /// definition of the `function_clause` raise — immediately before the
    /// `throw_with_trace` that lands it, and consumed by that call (see
    /// [`Machine::raise_function_clause_args`]). It is the failed function's **real**
    /// call arguments (`x(0..arity)` at the `func_info`/`test`/`select_val` raise
    /// site), materialized as a proper list — C's `make_arglist(c_p, reg, a)`
    /// (`../../erts/emulator/beam/beam_common.c`, `non_bif_stacktrace:`) — because
    /// C's cooked stacktrace item for the failing function carries `args` in its `A`
    /// slot, not the declared arity — C's `erts_build_mfa_item`
    /// (`../../erts/emulator/beam/utils.c:3430-3457`) emits the argument list when it
    /// is a list/nil — so the oracle cooks `{Mod, Fun, [2], Location}` for the failed
    /// function while every outer frame keeps its declared arity.
    ///
    /// It is a separate latch from the live-raise state so a propelling `Err` path —
    /// a raise with no live catch frame, which still reaches `throw_with_trace` —
    /// cannot leave a stale list for a later, unrelated cooked trace to inherit.
    pub(crate) pending_function_clause_args: Option<Eterm>,
    /// The **live raise** state behind [`Machine::pending_function_clause_args`]: the
    /// failed clause's argument list while the raise currently in flight was raised
    /// by the `function_clause` site.
    ///
    /// [`Machine::throw_with_trace`] sets it (from the one-shot latch) on **every**
    /// raise, so a raise that carries no such list clears it, and the two
    /// cooked-trace builders consume it **exactly once** — the caught path
    /// ([`crate::families::exceptions::stacktrace::symbolic_stacktrace`], which gives
    /// the list to the FIRST frame's `A` slot) and the uncaught path
    /// ([`Machine::with_uncaught_frames`]).
    ///
    /// The term is a machine-held heap term **outside** the frozen root set
    /// ([`crate::context::RootSet`]), so [`Machine::with_roots`] closes that window:
    /// for the duration of the root hand-off the latch rides at the tail of the
    /// Erlang stack (part of the published root set, rooted in full) and the field is
    /// **re-seated** from that slot afterwards, so a collecting host relocates the
    /// term and this field reads the *moved* word back.
    pub(crate) raise_function_clause_args: Option<Eterm>,
    /// A **one-shot latch**: the symbolic [`StackTrace`] an incoming raise
    /// **arrived carrying** (e.g. `erlang:raise/3` whose third argument is a
    /// non-empty trace list).
    ///
    /// Armed by the raising site ([`Machine::raise_error`]) **immediately before**
    /// the `throw_with_trace` that lands the raise, and consumed by that call (see
    /// [`Machine::raise_supplied_trace`]) — the same two-latch protocol as
    /// `Machine::pending_callee_head`/`pending_function_clause_args`. It is armed
    /// **only when the incoming `Raise`'s stacktrace is non-empty**: an empty or
    /// absent supplied trace leaves every path byte-identical to the
    /// `raise_snapshot`-derived behavior.
    ///
    /// It is a separate latch from the live-raise state so a propelling `Err` path
    /// — a raise with no live catch frame — cannot leave stale frames for a later,
    /// unrelated cooked trace to inherit.
    pub(crate) pending_supplied_trace: Option<StackTrace>,
    /// The **live raise** state behind [`Machine::pending_supplied_trace`]: the
    /// explicit frames the raise currently in flight arrived carrying.
    ///
    /// [`Machine::throw_with_trace`] sets it (from the one-shot latch) on **every**
    /// raise, so a raise that carries no supplied trace clears it, and the
    /// caught-path cooked-trace builder consumes it **exactly once**
    /// ([`crate::families::exceptions::stacktrace::symbolic_stacktrace`]), which
    /// then renders THOSE frames in preference to the `raise_snapshot` heuristic.
    ///
    /// The frames are plain metadata ([`beam_common::StackFrame`] values), never a
    /// machine-held heap term, so they need no rooting across the cooked trace's
    /// reservation.
    pub(crate) raise_supplied_trace: Option<StackTrace>,
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
    /// The `(payload base, capacity in bytes)` pairs of the off-heap payloads this
    /// machine allocated **for appending** — the `bs_init_writable` accumulator's
    /// backing store (C's `BIN_FLAG_WRITABLE` binary).
    ///
    /// **Why a side list.** The frozen `[BinRef][ErlSubBits]` pair records only a pointer
    /// and a logical bit window, so the one fact an in-place `private_append` needs on its
    /// fast path — "this payload is ours, it is writable, and it is `n` bytes wide" — has to
    /// live beside the machine. [`beam_alloc::RefcBinary`] now carries `capacity`,
    /// `reserved_bytes` and `max_capacity` in its private header, so this registry is not
    /// standing in for a missing word: it caches the payload's **real** capacity in bytes,
    /// so an in-place append that already fits needs no header read at all. The list is keyed by
    /// the payload's **first data byte** (`beam_alloc::RefcBinary::data_ptr`, exactly
    /// the value the term's `BinRef.val` word holds), and only entries deposited by
    /// [`Machine::mark_writable`] are considered writable: a payload built by
    /// [`Machine::alloc_refc_binary`] for an ordinary binary is never a target, so its
    /// immutability is untouched.
    ///
    /// **Per-machine, deliberately.** C clears writability when a writable binary
    /// escapes into another process (`erts_pin_writable_binary`). A per-machine
    /// registry gives the engine the same property structurally: a payload another
    /// process's copy points at is not in *this* machine's list, and the copy that
    /// reaches another process carries a freshly built `ErlSubBits` whose window ends
    /// at or before this append's start (see
    /// [`crate::families::bitstrings::raw::InPlace`] for the invariant that makes an
    /// in-place append invisible to every pre-existing alias).
    ///
    /// **Never cleared by [`Machine::reset`]**, for the same reason
    /// [`Machine::refc_binaries`] is not: an accumulator term may outlive the run that
    /// built it (C keeps such a binary in `p->wrt_bins`, writable, until it is pinned),
    /// and a registry entry must never outlive the payload it names — clearing the list
    /// while the payload stays alive would only lose the optimization, so the two
    /// rosters move together.
    pub(crate) writable_payloads: Vec<(usize, usize)>,
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
    /// interpreted instruction (before the instruction's effect) and offers each
    /// charge to the host through
    /// [`ExecutionContext::reduce`](crate::context::ExecutionContext::reduce) — a
    /// `&mut dyn ExecutionContext` vtable crossing the interpreter cannot elide. When
    /// the host's [`wants_reduce`](crate::context::ExecutionContext::wants_reduce) is
    /// `false` (its `reduce` is provably the default no-op), the driver skips that
    /// per-instruction call so the hot path pays a single `bool` load instead.
    ///
    /// The default is **conservative `true`**: a host that pre-empts by returning an
    /// error from `reduce` is never silently skipped. The slice counter
    /// ([`Machine::reductions`](crate::machine::Machine::reductions)) is bumped on
    /// every interpreted instruction regardless of this flag (and, on the native
    /// tiers, on every driver-loop iteration — the same gate, latched by that driver
    /// at its own entry), so opting out changes **only** whether the host hook is
    /// called, never the observable count.
    ///
    /// Latched at a run/resume entry exactly like [`Machine::trace_on`], and
    /// [`Machine::reset`](crate::machine::Machine::reset) leaves it `false`, so a run
    /// that never latches it (e.g. a direct `Machine::step_instruction` call in a
    /// test) cannot leak a stale value from an earlier run.
    pub(crate) reduce_on: bool,
    /// The **per-run in-machine BIF-dispatch gate**: the cached answer of
    /// [`ExecutionContext::wants_bif_in_machine`](crate::context::ExecutionContext::wants_bif_in_machine)
    /// for the run currently executing.
    ///
    /// When it is `true` the engine routes every BIF dispatch through
    /// [`ExecutionContext::call_bif_in_machine`](crate::context::ExecutionContext::call_bif_in_machine)
    /// (handing the host the live `&Machine` and the run's entry module) instead of
    /// [`ExecutionContext::call_bif`](crate::context::ExecutionContext::call_bif);
    /// when it is the default `false` the plain `call_bif` path is taken
    /// **byte-identically** and a dispatch pays a single `bool` load instead of an
    /// extra vtable crossing.
    ///
    /// Latched at a run/resume entry exactly like [`Machine::trace_on`]/
    /// [`Machine::reduce_on`] — by [`Machine::drive`](crate::machine::Machine) for the
    /// interpreter **and** by the native baseline tier's own driver for
    /// [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit), so the door is taken on
    /// every tier — and [`Machine::reset`](crate::machine::Machine::reset)
    /// leaves it `false`, so a run that never latches it (e.g. a direct
    /// `Machine::step_instruction` call in a test) cannot leak a stale value from an
    /// earlier run.
    pub(crate) bif_in_machine_on: bool,
    /// The code address the machine is currently executing, or was most recently
    /// **suspended** at: `(function index, code offset)`.
    ///
    /// Updated by [`Machine::drive`](crate::machine::Machine) **before** it prices
    /// each instruction, and again by
    /// [`Machine::step_instruction`](crate::machine::Machine) at its entry — the
    /// single choke point every *interpreted* instruction passes through (the
    /// interpreter's dispatch loop and the native tiers' per-instruction deopt
    /// helper both go through it) — and cleared by
    /// [`Machine::reset`](crate::machine::Machine::reset). `None` before the
    /// machine has interpreted anything.
    ///
    /// The store in the driver is what makes the pre-emption point sound: an `Err`
    /// from [`reduce`](crate::ExecutionContext::reduce) leaves the machine parked on
    /// the **not-yet-executed** instruction's address, so a resume at this position
    /// applies that instruction exactly once.
    ///
    /// This is the engine's read-only "saved instruction pointer": the input to
    /// the execution-position introspection ([`Machine::current_code`], and the
    /// derived [`Machine::current_function`]/[`Machine::current_location`]/
    /// [`Machine::current_stacktrace`]). Every **blocking** instruction
    /// (`wait`/`wait_timeout`) is interpreted rather than inlined by a native
    /// tier, so the recorded address is exact at every suspension point; on the
    /// native baseline tier the driver publishes each **block entry** before it is
    /// entered, so mid-block the recorded address is that block's entry (the address a
    /// pre-empted machine resumes at), never a fabricated mid-block one.
    pub(crate) current_code: Option<(u32, u32)>,
    /// The `(function index, arity)` this run was **entered** at — the engine's
    /// analogue of ERTS's `$initial_call`.
    ///
    /// Recorded once per run by [`Machine::run_loop`](crate::machine::Machine)
    /// (the first function entered after a [`Machine::reset`](crate::machine::Machine::reset),
    /// which clears it) and read back by [`Machine::initial_call`]. `None` before
    /// any run.
    pub(crate) entry_call: Option<(u32, u32)>,
    /// The number of **charges the current scheduling slice took** — the engine half of
    /// the runtime's *reduction* accounting, in the charging tier's own unit.
    ///
    /// Zeroed at every slice entry — both drivers ([`Machine::drive`](crate::machine::Machine)
    /// and the native baseline tier's driver loop) do it, since one `drive` call is one
    /// run or one resume, i.e. one slice — and incremented in the same step as the host
    /// hook, **before** the thing being priced runs, so
    /// [`Machine::reductions`](crate::machine::Machine) read when the slice ends is that
    /// slice's count with the raising unit and the charge a host pre-empted on included.
    ///
    /// The engine's other charging point is
    /// [`ensure_heap`](crate::ExecutionContext::ensure_heap) (reached by
    /// `test_heap`/`allocate_heap`), which prices *heap admissions* — a tight
    /// **non-allocating** loop never reaches it, which is why the per-instruction
    /// charge exists. A host observes the charge **during** the slice through
    /// [`reduce`](crate::ExecutionContext::reduce) and may pre-empt there; it never
    /// has to poll this field mid-run.
    ///
    /// Both tiers' drivers charge, each in its own unit. The **interpreter's** dispatch
    /// loop ([`Machine::drive`](crate::machine::Machine)) charges once per interpreted
    /// instruction; the **native baseline tier's** driver loop charges once per
    /// driver-loop iteration — one block entry, or the per-instruction interpreter step
    /// it takes for an address no compiled block covers — *before* the block is entered,
    /// so a refusing charge parks the machine at that block entry and the resume replays
    /// no prefix of it. A natively **linked** chain (a tail-call loop) therefore pays once
    /// per driver round-trip rather than per iteration: the count is exact in each tier's
    /// unit, a lower bound in *instructions* under the native tier, and never a fabricated
    /// number. The hook always sees the same charge (`tests/jit_reductions.rs`).
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
            x_mark: 0,
            fr_mark: 0,
            regs_escaped: false,
            stack: Vec::new(),
            frames: Vec::new(),
            catch_frames: Vec::new(),
            pending_exception: None,
            class_atoms: UNRESOLVED_CLASS_ATOMS,
            class_atoms_live: false,            max_register,
            literals: None,
            entry_literals: None,
            contexts: Contexts::new(),
            context: 0,
            raise_snapshot: Vec::new(),
            pending_callee_head: false,
            raise_head_item: false,
            pending_function_clause_args: None,
            raise_function_clause_args: None,
            pending_supplied_trace: None,
            raise_supplied_trace: None,
            refc_binaries: Vec::new(),
            writable_payloads: Vec::new(),
            args_scratch: Vec::new(),
            trace_on: false,
            reduce_on: false,
            bif_in_machine_on: false,
            current_code: None,
            entry_call: None,
            reductions: 0,
        };
        machine.reset();
        machine
    }

    /// The number of registers in **this** machine's X-register file
    /// (`x[0..register_count())`).
    ///
    /// It is the bound [`Machine::x`] indexes into, so a register index is readable
    /// exactly when it is `< register_count()`. Reporting the machine's own size —
    /// rather than the [`MAX_REGISTER`] constant — is what lets a caller that walks
    /// the argument registers stop at the ones that exist when the file was narrowed
    /// with [`Machine::with_max_register`].
    #[inline]
    pub fn register_count(&self) -> u32 {
        self.x.len() as u32
    }
    /// Bind the literal area of the module context about to be executed.    ///
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
    ///
    /// # The machine-held heap terms `f` also roots
    ///
    /// A raise parks heap terms in `Machine` fields that are **not** part of the
    /// frozen [`RootSet`]: the two failed-clause argument latches
    /// ([`Machine::pending_function_clause_args`] — armed immediately before the
    /// `throw_with_trace` that lands the raise, so never live across an allocation —
    /// and [`Machine::raise_function_clause_args`] — the **live raise** state the
    /// cooked-trace builders read *after* their rooted reservation). A collecting
    /// host must relocate those terms too, so this method **parks** whichever of the
    /// two is armed at the end of the **Erlang stack** for the duration of `f` and
    /// **re-seats** each field from its slot afterwards — the stack is part of the
    /// frozen root set and is rooted in full (there is no liveness map over Y slots),
    /// so a conforming host that only scans `x[0..live_len()]` roots the parked term
    /// regardless of the `live` hint. The parked words lengthen `roots.stack` by at
    /// most two; over-approximating a root set is always safe for a copying
    /// collector, and the length is restored before this method returns, so no
    /// engine state observes the temporary words.
    ///
    /// When no latch is armed (the overwhelming majority of hand-offs) the stack is
    /// untouched and this method is byte-identical to the pre-parking behavior.
    ///
    /// [`Machine::pending_exception`]'s reason term is deliberately **not** parked
    /// here: it is a copy of the word `land_exception` publishes in `x(1)`/`x(3)`,
    /// so it is already relocated through the ordinary root set on the caught path
    /// (and rides out of the machine in the raise's live-reason slot on the uncaught
    /// path, never re-read from the field).
    pub fn with_roots<R>(&mut self, live: u32, f: impl FnOnce(RootSet<'_>) -> R) -> R {
        // The receiver is handed mutable slices of BOTH register files, so it may write
        // any slot of either — a move that the `set_x`/`set_fr` high-water marks cannot
        // see. Mark the files escaped so the next `reset` refills them in full rather
        // than trusting the marks.
        self.regs_escaped = true;
        let raise_args = self.raise_function_clause_args;
        let pending_args = self.pending_function_clause_args;
        if raise_args.is_none() && pending_args.is_none() {
            let roots = RootSet::new(&mut self.x, &mut self.stack, &mut self.fr, live);
            return f(roots);
        }
        // A raise is in flight and holds at least one heap-term latch: park them so the
        // host roots (and relocates) them, then re-seat them from the slots the host
        // wrote back through.
        let base = self.stack.len();
        if let Some(term) = raise_args {
            self.stack.push(term);
        }
        if let Some(term) = pending_args {
            self.stack.push(term);
        }
        let result = {
            let roots = RootSet::new(&mut self.x, &mut self.stack, &mut self.fr, live);
            f(roots)
        };
        let mut cursor = base;
        if raise_args.is_some() {
            self.raise_function_clause_args = self.stack.get(cursor).copied();
            cursor += 1;
        }
        if pending_args.is_some() {
            self.pending_function_clause_args = self.stack.get(cursor).copied();
        }
        self.stack.truncate(base);
        result
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
    /// The payload is `beam_alloc`'s `binary_alloc` allocation. It is **immutable by
    /// default**: it is allocated with no spare room (`len == capacity`), so a caller
    /// that needs a larger binary builds a new one. The one exception is a payload the
    /// caller then hands to [`Machine::mark_writable`] (the `bs_init_writable`
    /// accumulator), which the bitstring family appends into **up to its capacity** —
    /// never past it, and never at a bit offset below the accumulator's own window end —
    /// and may widen through [`Machine::grow_writable_payload`]. That growth door is why
    /// an *accumulator* payload is allocated by
    /// [`Machine::alloc_refc_binary_with_capacity`] instead: only a capacity-carrying
    /// payload (spare reservation included) can grow.
    pub(crate) fn alloc_refc_binary(&mut self, bytes: &[u8]) -> Result<*mut u8> {
        let binary = beam_alloc::RefcBinary::new(bytes)?;
        let data = binary.data_ptr();
        self.refc_binaries.push(binary);
        Ok(data)
    }

    /// Allocate a **capacity-carrying** off-heap binary payload for appending and return
    /// `(first data byte, capacity in bytes)` — the writable-accumulator constructor
    /// (`bs_init_writable`; its consumer is
    /// `families::bitstrings::raw::make_writable`).
    ///
    /// Unlike [`Machine::alloc_refc_binary`] — which copies `bytes` into an
    /// exactly-sized, **immutable** payload (`len == capacity == reserved`) — this
    /// allocates through [`beam_alloc::RefcBinary::with_capacity`]: the writable capacity
    /// is `capacity` bytes, backed by a reservation of
    /// `round_up_unit(capacity * DEFAULT_RESERVATION_FACTOR)` (4x, clamped to
    /// `MAX_PAYLOAD_BYTES`). That capacity word is exactly what makes an appending
    /// `private_append` loop **amortised linear** instead of O(n²): the sink writes into
    /// the spare reservation and [`Machine::grow_writable_payload`] only buys memory at
    /// the reservation's edge.
    ///
    /// The payload is returned **fully live** (`len == capacity`, zero-filled): an
    /// appender writes through the payload pointer rather than through the handle, so a
    /// handle whose `len` lagged behind the written bytes would lose them on a relocation
    /// ([`Machine::cover_writable_capacity`] has the argument and the C correspondence —
    /// `bin_size` is the whole backing store, `apparent_size` is the term's window, which
    /// for a fresh `bs_init_writable` accumulator is 0 bits).
    ///
    /// `capacity == 0` is legal and needs no clamp: `RefcBinary::ensure_capacity(1)`
    /// makes progress from a zero capacity (`grow_span(0) == 0`, but the request itself
    /// is the target, so the payload is relocated to a `UNIT_SIZE` reservation and the
    /// capacity rises to 1) — verified against `beam_alloc/src/binary/growth.rs`, so no
    /// zero-progress hazard exists on the growth path.
    ///
    /// The returned capacity is the payload's **real** capacity, read back from its
    /// control header rather than echoing the requested hint, so a caller that keys the
    /// writable registry on it (`Machine::mark_writable`) can never disagree with what
    /// the payload actually offers. `RefcBinary::with_capacity` currently sets
    /// `capacity() == capacity`, so this is an invariant check as much as a read.
    ///
    /// The handle joins [`Machine::refc_binaries`] (which [`Machine::reset`] never
    /// clears), so the payload stays alive and address-stable until the machine drops.
    pub(crate) fn alloc_refc_binary_with_capacity(
        &mut self,
        capacity: usize,
    ) -> Result<(*mut u8, usize)> {
        let mut binary = beam_alloc::RefcBinary::with_capacity(capacity)?;
        let real_capacity = binary.capacity();
        debug_assert_eq!(
            real_capacity, capacity,
            "`with_capacity` hands back the capacity it was asked for"
        );
        // The whole backing store is live from the start (C's `bin_size`), so a later
        // relocation copies every byte an appender writes through the payload pointer,
        // and `as_bytes()` never exposes uninitialised off-heap memory. See
        // [`Machine::cover_writable_capacity`] for the whole argument.
        if real_capacity > 0 {
            binary.set_len(real_capacity)?;
        }
        let data = binary.data_ptr();
        self.refc_binaries.push(binary);
        Ok((data, real_capacity))
    }

    /// Record that the off-heap payload starting at `base` was allocated **for
    /// appending** and is `capacity_bytes` bytes wide — the writable-accumulator half
    /// of a `bs_init_writable`/`private_append` build (C's `BIN_FLAG_WRITABLE` binary,
    /// `erl_bits.c:1684-1720`).
    ///
    /// Only a payload registered here may be written in place: the lookup
    /// [`Machine::writable_capacity`] is the gate
    /// [`crate::families::bitstrings::raw::writable_append_target`] consults, so an
    /// ordinary refc binary (`Machine::alloc_refc_binary`, never registered) keeps its
    /// immutability. The deposit is a plain push — no BEAM-heap allocation, no
    /// collection, no root set involved (`base` is an off-heap address, never a term).
    ///
    /// A duplicate registration of the same base is impossible for a live payload
    /// (each `bs_init_writable` allocates a fresh one and the handle roster keeps it
    /// alive), and the lookup prefers the newest entry anyway.
    pub(crate) fn mark_writable(&mut self, base: *mut u8, capacity_bytes: usize) {
        self.writable_payloads.push((base as usize, capacity_bytes));
    }

    /// The capacity, in **bytes**, of the writable payload whose first data byte is
    /// `base` — `Some` only for a payload this machine allocated for appending
    /// ([`Machine::mark_writable`]).
    ///
    /// A linear scan of a per-process list that holds one entry per live
    /// `bs_init_writable` accumulator (a handful, in practice), so it allocates
    /// nothing and cannot collect.
    pub(crate) fn writable_capacity(&self, base: *const u8) -> Option<usize> {
        let key = base as usize;
        self.writable_payloads
            .iter()
            .rev()
            .find(|(registered, _)| *registered == key)
            .map(|(_, capacity)| *capacity)
    }

    /// Re-key (and re-size) the writable-payload registry entry that currently names
    /// `old_base`, so it names `new_base` with `capacity_bytes`.
    ///
    /// The registry's `(base, capacity in bytes)` shape and its **newest-entry-preferred**
    /// lookup are unchanged — only the matching entry's two words move — so a payload
    /// whose block was reallocated (a [`PayloadGrowOutcome::Moved`],
    /// [`Machine::grow_writable_payload`]) stays the one writable payload it was, and a
    /// stale key can never linger to answer for a freed block.
    ///
    /// `false` means there was no entry for `old_base` at all (the payload was never
    /// registered for appending), in which case nothing is inserted: this is a *re-key*,
    /// never a registration.
    fn rekey_writable(
        &mut self,
        old_base: *mut u8,
        new_base: *mut u8,
        capacity_bytes: usize,
    ) -> bool {
        let key = old_base as usize;
        // Newest-entry-preferred, exactly like `writable_capacity`, so the re-key and
        // the lookup can never pick different entries.
        match self
            .writable_payloads
            .iter_mut()
            .rev()
            .find(|(registered, _)| *registered == key)
        {
            Some(entry) => {
                entry.0 = new_base as usize;
                entry.1 = capacity_bytes;
                true
            }
            None => false,
        }
    }

    /// Declare the **whole capacity** of the writable payload whose first data byte is
    /// `base` live (`len == capacity`) — the length invariant a writable accumulator's
    /// payload carries, restored after every growth.
    ///
    /// # Why `len` must cover the capacity
    ///
    /// `beam_alloc`'s relocation copies exactly `RefcBinary::len()` payload bytes
    /// (`binary/growth.rs::relocate`), and `RefcBinary::as_bytes()` exposes exactly that
    /// many. An appender writes **through the payload pointer** (`InPlace` holds no
    /// handle), so the handle's own `len` never follows those writes by itself: without
    /// this invariant a move would silently drop everything the appender wrote, and
    /// `as_bytes()` would under-report the bytes the payload really holds (`beam_gc`'s
    /// reachability accounting reads it).
    ///
    /// `len == capacity` is C's `bin_size` — the whole backing store of a *writable*
    /// binary (`erl_bits.c:2023`), as opposed to the term's `apparent_size` (the
    /// `ErlSubBits` window, which is what actually bounds the visible bits). The region
    /// `[0, capacity)` is therefore always initialised: zero-filled here, then written by
    /// the appender.
    ///
    /// [`RefcBinary::set_len`](beam_alloc::RefcBinary::set_len) zero-fills the range it
    /// newly exposes, so a caller may only extend over bytes **no writer has reached**.
    /// Both call sites satisfy that by construction: at construction the payload is empty,
    /// and after a growth the exposed range is exactly the capacity just committed — an
    /// appender can only write past the *previous* capacity once that growth returned.
    ///
    /// Returns `Ok(false)` — nothing changed — when `base` names no payload of this
    /// machine, or one it never registered for appending (the same refusals
    /// [`Machine::grow_writable_payload`] documents: only this machine's own writable
    /// accumulators have their length managed here).
    ///
    /// # Errors
    ///
    /// The typed error `beam_alloc` reports when the payload is still **shared**
    /// ([`RefcBinary::set_len`](beam_alloc::RefcBinary::set_len)'s uniqueness
    /// precondition). Only [`Machine::grow_writable_payload`] can observe it, and it folds
    /// it into its documented `Ok(None)`; the construction-time call cannot (the machine
    /// holds the only handle).
    pub(crate) fn cover_writable_capacity(&mut self, base: *mut u8) -> Result<bool> {
        let Some(index) = self
            .refc_binaries
            .iter()
            .position(|binary| binary.data_ptr() == base)
        else {
            return Ok(false);
        };
        if self.writable_capacity(base).is_none() {
            return Ok(false);
        }
        let binary = &mut self.refc_binaries[index];
        let capacity = binary.capacity();
        if binary.len() == capacity {
            return Ok(true);
        }
        binary.set_len(capacity)?;
        Ok(true)
    }

    /// Grow the **writable** off-heap payload whose first data byte is `base` so that it
    /// offers at least `wanted_bytes` writable bytes, re-keying the writable registry to
    /// whatever the growth did.
    ///
    /// This is the payload half of the capacity adoption: the engine's writable sink
    /// ([`crate::families::bitstrings::raw::InPlace`]) asks for room, `beam_alloc` decides
    /// in place vs. move (`RefcBinary::ensure_capacity`'s `GROW_PROC_BIN_SIZE` schedule),
    /// and this door keeps the machine's own two books straight:
    ///
    /// * [`PayloadGrowOutcome::InPlace`] — the payload's address is unchanged, so only the
    ///   registry's cached capacity moves. `data_ptr()` stayed put, so **no term needs
    ///   republishing** and every `BinRef.val` is still valid.
    /// * [`PayloadGrowOutcome::Moved`] — the payload was reallocated and its old block
    ///   **freed**, so the registry is re-keyed to [`PayloadRelocation::to`] and that new
    ///   address is returned. Republishing the on-heap `[BinRef][ErlSubBits]` pair(s) over
    ///   it is the *caller's* job (it owns the term; this door is safe Rust and never
    ///   dereferences a term) — see `InPlace::grow`'s `SAFETY` contract.
    ///
    /// Returns `Ok(Some((base, capacity_bytes)))` when the payload now offers
    /// `wanted_bytes`. Returns `Ok(None)` — "cannot grow, fall back to the copy-rebuild
    /// form" — for **every** refusal, each of which is a legitimate runtime condition
    /// rather than a programming error:
    ///
    /// * `base` names no payload in [`Machine::refc_binaries`] (not ours — e.g. a payload
    ///   built by another machine or by a host);
    /// * `base` names a payload that was **not** registered for appending
    ///   ([`Machine::writable_capacity`] is `None`), so its immutability is untouched;
    /// * `beam_alloc` refused the growth: the payload is **shared**
    ///   (`refcount() != 1` — a host or another process adopted a handle, exactly the
    ///   situation C resolves with `erts_pin_writable_binary`), or the request exceeds the
    ///   payload's `max_capacity` (typed `AllocKind::LargeObject`), or the allocator could
    ///   not serve the block.
    ///
    /// The copy-rebuild form is always byte-correct, so a refusal costs performance only.
    /// No `Err` is produced for any of those: a payload that cannot grow is a *runtime
    /// condition*, not an internal fault, and the caller already owns the correct fallback
    /// for everything this door reports. The single `?` is [`Machine::cover_writable_capacity`]'s
    /// uniqueness error, which cannot fire right after `ensure_capacity` required a unique
    /// handle — it is surfaced rather than swallowed because reaching it would mean an
    /// internal invariant broke, and the old block is already gone by then.
    ///
    /// Both outcomes also restore the writable-payload length invariant
    /// (`len == capacity`, [`Machine::cover_writable_capacity`]) **before** returning, so
    /// the next relocation copies every byte the appender wrote through the payload
    /// pointer.
    pub(crate) fn grow_writable_payload(
        &mut self,
        base: *mut u8,
        wanted_bytes: usize,
    ) -> Result<Option<(*mut u8, usize)>> {
        let Some(index) = self
            .refc_binaries
            .iter()
            .position(|binary| binary.data_ptr() == base)
        else {
            return Ok(None);
        };
        if self.writable_capacity(base).is_none() {
            return Ok(None);
        }
        let outcome = match self.refc_binaries[index].ensure_capacity(wanted_bytes) {
            Ok(outcome) => outcome,
            // Shared payload / `max_capacity` cap / allocation failure: the payload is
            // unchanged (`ensure_capacity`'s documented guarantee), so the caller's
            // copy-rebuild fallback is the only remaining route.
            Err(_) => return Ok(None),
        };
        let capacity = self.refc_binaries[index].capacity();
        let new_base = match outcome {
            beam_alloc::PayloadGrowOutcome::InPlace { .. } => base,
            beam_alloc::PayloadGrowOutcome::Moved(relocation) => relocation.to(),
        };
        // Re-key first, so the invariant door below finds the payload under the address it
        // now has.
        self.rekey_writable(base, new_base, capacity);
        // Restore the writable-payload length invariant *before* returning: the bytes an
        // appender has already written live at `[0, min(written, previous capacity))`, and
        // a `Moved` relocation just copied exactly `len() == previous capacity` bytes, so
        // they survived. Extending the length over the capacity this growth just committed
        // only zero-fills a region no writer can have reached yet (the appender writes past
        // the previous capacity only after this call returns) — see
        // [`Machine::cover_writable_capacity`]. Its uniqueness error cannot fire here:
        // `ensure_capacity` just required the same unique handle.
        let covered = self.cover_writable_capacity(new_base)?;
        debug_assert!(covered, "the payload this door just grew is registered writable");
        Ok(Some((new_base, capacity)))
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
