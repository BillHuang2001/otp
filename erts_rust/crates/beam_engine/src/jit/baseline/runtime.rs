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

//! The baseline JIT's block ABI and its per-instruction helper.
//!
//! The `HALT`/`ERROR`/`WAIT` return words and the `(func, pc)` control-word codec,
//! the `#[repr(C)]` runtime block a compiled block receives, and the helpers a
//! block calls: [`jit_step_one`] — the per-instruction deopt for the first
//! instruction the block could not lower natively (a suspending `receive` is handed
//! back to the driver through [`WAIT_BIT`], so a blocking-aware driver can report it
//! as a suspension) — [`jit_ext_call`], the native arm of a `call_ext*` (the import
//! resolution plus the variant's frame discipline, handed to the driver as the
//! [`ENTER`] word) — and [`jit_tail_call`], the checked frame pop a **linked local
//! tail call** uses (see [`super::emit::link`]). See `super` for the block ABI as a
//! whole, the emitter that produces blocks and the driver that decodes the words
//! these items produce.
#![allow(unsafe_code)]

use crate::interpreter::{ImportFrame, LabelMap};
use std::mem;

use beam_common::BeamError;
use beam_loader::{ImportRef, Instr, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::{Continuation, Machine};
use crate::Step;

use super::code::CompiledModule;
// ---------------------------------------------------------------------------
// The block ABI
// ---------------------------------------------------------------------------

/// The `HALT` block-return word: the run finished, the driver yields `x(0)`.
///
/// A negative word is never a valid [`encode_target`] result (the encoding is
/// `(func << 32) | pc` with `func < 2^31`, so its sign bit is clear).
pub const HALT: i64 = -1;

/// The `ERROR` block-return word: a typed error was stored in the runtime's
/// `error` slot and the driver must propagate it.
pub const ERROR: i64 = -2;

/// The `ENTER` block-return word: the block's per-instruction deopt hit a
/// module-context transfer ([`Step::Enter`]) and stored the target in the runtime's
/// [`JitRuntime::enter`] slot.
///
/// A compiled block can only address **one** module's code, label map and literal
/// area, so a transfer into another module is a *driver* action: the block hands the
/// resolved `(context, func, pc)` back and the driver performs the switch exactly as
/// [`Machine::drive`](crate::Machine) does, then re-enters native code in the new
/// module. Like [`HALT`]/[`ERROR`] the word carries no address, and it is negative so
/// it can never be confused with a real [`encode_target`] continuation.
pub const ENTER: i64 = -3;

/// The `SUSPEND` block-return word: the block's per-instruction deopt hit a BIF that
/// answered [`BifOutcome::Suspend`](crate::BifOutcome::Suspend)
/// ([`Step::Suspend`]), and stored the **un-applied instruction's** `(func, pc)` in the
/// runtime's [`JitRuntime::suspend`] slot.
///
/// A compiled block owns no host state and cannot park the process itself, so the word
/// is a *driver* action exactly like [`ENTER`]: the driver takes the address from the
/// side channel and reports [`crate::RunOutcome::Suspend`], leaving the register file
/// and the frames untouched (the block's exit spilled its cached registers first, so
/// the machine is exactly the pre-execution state a re-execution needs). Like
/// [`HALT`]/[`ERROR`]/[`ENTER`] the word carries no address, and it is negative so it
/// can never be confused with a real [`encode_target`] continuation.
pub const SUSPEND: i64 = -5;
/// Encodes a `(func, pc)` continuation as the block-return word.
///
/// `func` is the function index into [`Module::functions`], `pc` the code offset
/// within it. The result is non-negative for every `func < 2^31` (which is every
/// module an engine can hold: the entry table is keyed by `u32` but the ABI keeps
/// the top bit for the negative sentinels).
pub const fn encode_target(func: u32, pc: u32) -> i64 {
    ((func as i64) << 32) | (pc as i64 & 0xFFFF_FFFF)
}

/// Decodes a block-return word into its `(func, pc)` continuation, or `None` for a
/// sentinel ([`HALT`]/[`ERROR`]) — or any other negative word, which no block the
/// compiler emits can produce.
pub const fn decode_target(word: i64) -> Option<(u32, u32)> {
    if word < 0 {
        return None;
    }
    Some(((word >> 32) as u32, (word & 0xFFFF_FFFF) as u32))
}

/// The sign bit of a [`Step::Wait`] block-return word: the marker a suspending
/// `receive` sets on its encoded resume address (see [`encode_wait`]).
///
/// A [`Step::Wait`] fits neither existing word shape: [`HALT`]/[`ERROR`] carry no
/// address, and the plain non-negative [`encode_target`] word is a normal
/// continuation the driver would *follow* — for a `wait`'s target that is usually
/// the very `loop_rec` that just found the mailbox empty, so following it would
/// spin instead of reporting the suspension. It is therefore the resume address
/// encoded exactly as [`encode_target`] with the **sign bit** set, which no other
/// word uses ([`HALT`]/[`ERROR`] are compared first, and every real target is
/// non-negative because `func < 2^31`).
///
/// A **non-blocking** driver folds the decoded address back into the typed
/// [`crate::error::blocked`] error; a blocking-aware driver turns it into
/// [`crate::RunOutcome::Wait`].
pub const WAIT_BIT: i64 = i64::MIN;

/// Encodes a [`Step::Wait`] resume address as the block-return word (see
/// [`WAIT_BIT`]).
pub const fn encode_wait(func: u32, pc: u32) -> i64 {
    WAIT_BIT | encode_target(func, pc)
}

/// Decodes a [`Step::Wait`] block-return word into its `(func, pc)` resume address,
/// or `None` for every other word (a real continuation, [`HALT`], [`ERROR`],
/// [`ENTER`], [`LOCAL_DECLINE`] or [`SUSPEND`]).
pub const fn decode_wait(word: i64) -> Option<(u32, u32)> {
    if word >= 0
        || word == HALT
        || word == ERROR
        || word == ENTER
        || word == LOCAL_DECLINE
        || word == SUSPEND
    {
        return None;
    }
    decode_target(word & !WAIT_BIT)
}
/// The one argument a compiled block receives.
///
/// A `#[repr(C)]` bag of raw pointers at the live driver-owned objects (valid for
/// the whole driver call, never aliased while a `&mut` derived from a field is
/// alive) plus the slot a helper stores a propagating [`BeamError`] in. Generated
/// code touches **only the offsets named by the `RUNTIME_*_DISP` constants**
/// ([`RUNTIME_MACHINE_DISP`], [`RUNTIME_XREGS_DISP`], [`RUNTIME_YBASE_DISP`],
/// [`RUNTIME_YEND_DISP`], [`RUNTIME_LITERALS_DISP`], [`RUNTIME_LITERALS_END_DISP`]);
/// the remaining fields are read by Rust only (the driver and the
/// [`jit_step_one`] helper).
///
/// The two literal-area fields are appended **after** [`Self::enter`] so every
/// pre-existing `#[repr(C)]` offset is untouched.
///
/// `machine` is the [`Machine`] itself (what the helper needs); `x_regs` is the base
/// of its **x-register array** (`machine.x.as_ptr()`), which is what a block indexes.
/// The two are separate fields because a block's `[base + n*8]` addressing needs the
/// array's data pointer, not the address of the `Vec` field inside a non-`repr(C)`
/// Rust struct. `machine.x` is only resized by [`Machine::reset`] (which runs before
/// this pointer is taken) — `ensure_heap` hands the context a slice, so it cannot
/// reallocate the array mid-run.
///
/// `y_base`/`y_end` are the **current frame's Y window**: the base of the top call
/// frame's first Y slot (`machine.stack[y_base]`) and the one-past-the-last address of
/// its `y_count` slots. A block reads a Y operand and writes a Y destination through
/// them; a block that accesses a Y slot publishes the pair in its **prologue**
/// ([`jit_publish_y`]), and re-publishes it after an in-block helper call when a later
/// instruction of the block accesses a Y slot ([`jit_refresh_y`]) — the Erlang stack is
/// a Rust `Vec` that `push_frame` may reallocate, so the window must never outlive the
/// frame it names. When no frame is live both are null, which makes every emitted Y
/// access reject (see the Y-window guard in the arithmetic fast path).
///
/// The lifetime parameter bounds only the [`ExecutionContext`] trait object (it is
/// invariant, so the object's own lifetime must be named); it does not affect the
/// `#[repr(C)]` layout.
#[repr(C)]
pub(crate) struct JitRuntime<'a> {
    /// The process state a block reads and writes through (`stack`, frames, …).
    pub(crate) machine: *mut Machine,
    /// The base of the x-register array (`machine.x`) a block loads from / spills to.
    pub(crate) x_regs: *mut Eterm,
    /// The base of the current frame's first Y slot (`machine.stack[y_base]`).
    ///
    /// Null when no frame is live. Read by generated code at
    /// [`RUNTIME_YBASE_DISP`].
    pub(crate) y_base: *mut Eterm,
    /// One past the current frame's last Y slot (`y_base + y_count`).
    ///
    /// Null when no frame is live. Read by generated code at
    /// [`RUNTIME_YEND_DISP`].
    pub(crate) y_end: *mut Eterm,
    /// The module being executed (its `code`, labels, imports, literals).
    pub(crate) module: *const Module,
    /// The run's **entry module** (module context `0`) — the module the driver was
    /// entered at, distinct from [`Self::module`] only while a cross-module callee
    /// runs. It is what the in-machine BIF-dispatch door
    /// ([`ExecutionContext::call_bif_in_machine`]) and the raise funnel's
    /// context-`0` half name a context-`0` frame against, and it is fixed for the
    /// whole driver loop (a context switch only rebinds [`Self::module`]).
    pub(crate) entry_module: *const Module,
    /// The module-wide `Label -> (func, pc)` map the helper resolves fail/target
    /// labels through (built once per driver call by
    /// [`interpreter::build_label_map`](crate::interpreter)).
    pub(crate) labels: *const LabelMap,
    /// The [`ExecutionContext`] seam, handed back as `&mut dyn` to the helper.
    pub(crate) ctx: *mut (dyn ExecutionContext + 'a),
    /// Set by a helper that hit a propagating `Err`; the `ERROR` word tells the
    /// driver to take it.
    pub(crate) error: Option<BeamError>,
    /// The `(context, func, pc)` of a module-context transfer a helper reported with
    /// the [`ENTER`] word; the driver takes it, switches context and re-enters there.
    ///
    /// Read and written **only by Rust** (the driver and [`jit_step_one`]).
    pub(crate) enter: Option<(u32, u32, u32)>,
    /// The `(func, pc)` of the **un-applied** instruction a helper reported with the
    /// [`SUSPEND`] word; the driver takes it, reports
    /// [`crate::RunOutcome::Suspend`] and leaves the machine untouched so the host's
    /// resume re-executes that instruction.
    ///
    /// The [`JitRuntime::enter`] slot's twin: the block ABI has room for one small
    /// negative sentinel but not for an address beside it, so the address rides the
    /// runtime instead.
    ///
    /// Read and written **only by Rust** (the driver and the helpers), never by
    /// generated code; its position in the struct is immaterial because every offset
    /// emitted code uses is computed with [`mem::offset_of!`].
    pub(crate) suspend: Option<(u32, u32)>,
    /// The base of the **literal area** the machine currently has bound — the first
    /// word of `Module::literals.terms()` for the active module.
    ///
    /// A compiled block loads `Operand::Literal(n)` through this field
    /// (`[literals_base + n*8]`, behind the guard below) instead of materialising the
    /// literal as an immediate. That is a **correctness** requirement, not an
    /// optimisation: a compiled artifact is cached under a key derived from the
    /// module's **content** (`Module::checksum` / the decoded-IR fingerprint), so a
    /// cached block can outlive the particular `Arc<Module>` it was compiled for and
    /// be reused for a re-loaded, byte-identical module — whose literal area is a
    /// *different* allocation. A baked-in literal-area pointer (or a baked-in literal
    /// word, which is a pointer into that area) could therefore dangle. Loading from
    /// the live binding is what makes the tier exact.
    ///
    /// Null when no area is bound; [`Self::literals_end`] is null too then, which
    /// makes every emitted literal access reject (see the literal guard in
    /// [`super::emit`]) — the same "null means reject every access" discipline the
    /// Y window uses. Read by generated code at [`RUNTIME_LITERALS_DISP`].
    pub(crate) literals_base: *const Eterm,
    /// One past the last word of the bound literal area
    /// (`literals_base + terms().len()`), or null when none is bound.
    ///
    /// Read by generated code at [`RUNTIME_LITERALS_END_DISP`]: the emitted guard is
    /// `lea addr, [base + n*8]; cmp addr, end; jae reject`, so it rejects both an
    /// absent area (null base ⇒ `n*8 >= 0 == end`) and an index at/past the area's
    /// length — the runtime twin of the interpreter's
    /// [`Machine::literal`](crate::Machine::literal) bounds check, in the exact shape
    /// of the Y-window guard.
    pub(crate) literals_end: *const Eterm,
    /// The block entry a [`jit_return`]/[`jit_deallocate_return`] resolved **inside the
    /// function that is already running**, or null when the continuation must go back to
    /// the driver (a bottom frame's [`HALT`], another module context's [`ENTER`], a
    /// different function's mapping, an address no block covers — and every failing
    /// helper).
    ///
    /// Written by the two return helpers and read by the emitted arm that calls them,
    /// which tail-jumps to it (leaving the block's native frame first, so the target's own
    /// prologue rebuilds one) instead of returning a word for the driver to decode. The
    /// skipped driver iteration's work is performed by the helper *before* the transfer —
    /// see [`resume_in_block`] — so the resumed block is entered exactly as a
    /// driver-entered one. Read by generated code at [`RUNTIME_RESUME_DISP`]; always null
    /// for every other helper.
    pub(crate) resume_entry: *const u8,
    /// The block-entry tables of the function the driver last entered natively
    /// ([`EntryTables`]) — what [`resume_in_block`] resolves a continuation through first.
    ///
    /// Published by the driver at every driver-loop iteration in which it enters native
    /// code, and set to [`EntryTables::NONE`] at every iteration that takes an interpreter
    /// step instead, so it always names the function whose blocks may be executing. Read
    /// and written **only by Rust**.
    pub(crate) tables: EntryTables,
    /// The compiled artifact whose code is currently executing, or null when no native code
    /// is running — what [`resume_in_block`] resolves a **cross-function** continuation
    /// through (see [`CompiledModule::native_entry_tables`]).
    ///
    /// The driver owns the artifact for the whole slice (the caller-owned one in module
    /// context `0`, a callee's `Arc` held by its `CalleeCode`), so the pointer stays valid
    /// for as long as a block of it can be running and its code can never be unmapped
    /// mid-slice. Published on exactly the iterations that enter native code — the same
    /// ones that publish [`Self::tables`] — and read and written **only by Rust**.
    ///
    /// [`CompiledModule::native_entry_tables`]: super::code::CompiledModule::native_entry_tables
    pub(crate) compiled: *const CompiledModule,
    /// `&mut machine.current_code` — the position word a block's prologue stores its
    /// own `(func, pc)` into **without a helper call**.
    ///
    /// The same rationale as [`Self::x_regs`]: `Machine` is not `repr(C)`, and the
    /// field is a plain `u64` the driver never moves for the whole loop, so its
    /// address is published once at [`JitRuntime`] construction and the emitted
    /// prologue writes through it (`mov r11, movabs pack_current_code(func, pc)`;
    /// `mov [r10], r11`). Read by generated code at [`RUNTIME_CURRENT_CODE_DISP`];
    /// written **only** by generated code and by the driver (which still stores the
    /// same field directly at its loop head — see `Machine::set_current_code`).
    pub(crate) current_code: *mut u64,
}

/// The block-entry tables of one function, published to [`JitRuntime`] by the driver so a
/// **return** can resolve its own continuation without leaving the block.
///
/// The two tables are the artifact's own per-function entry vectors
/// ([`CompiledFunction::entries`]/[`CompiledFunction::sub_entries`] for the lazy layout,
/// the `[func]` slices of `EagerLayout::entries`/`sub_entries` for the eager one), read
/// through raw pointers: a compiled function's entry vectors are built once, before its
/// buffer is sealed, and are never mutated afterwards, and the artifact that owns them
/// outlives every run (the driver holds the artifact, and the lazy layout's `RefCell` slot
/// keeps its own `Rc`), so the pointers stay valid for as long as the driver can consult
/// them.
///
/// [`CompiledFunction::entries`]: super::code::CompiledFunction
#[derive(Clone, Copy)]
pub(crate) struct EntryTables {
    /// The function these tables belong to — a continuation in **another** function is a
    /// different mapping, which only the driver may switch to.
    pub(crate) func: u32,
    /// `entries[pc]`: the block's native entry, or `None` for an address this function
    /// compiled no block start for.
    pub(crate) entries: *const Option<*const u8>,
    /// The number of slots in `entries` (the function's code length).
    pub(crate) entries_len: usize,
    /// `sub_entries[pc]`: the **continuation sub-block** starting at `pc`, or `None`.
    pub(crate) sub_entries: *const Option<*const u8>,
    /// The number of slots in `sub_entries`.
    pub(crate) sub_entries_len: usize,
}

impl EntryTables {
    /// "No tables" — the value published when the driver enters no native code.
    ///
    /// `func` is `u32::MAX`, which no real function index can be (a `Module`'s function
    /// list is far shorter, and the block ABI reserves `func < 2^31`), so a lookup against
    /// it never matches.
    pub(crate) const NONE: EntryTables = EntryTables {
        func: u32::MAX,
        entries: std::ptr::null(),
        entries_len: 0,
        sub_entries: std::ptr::null(),
        sub_entries_len: 0,
    };

    /// The tables of `func` from that function's own two entry vectors.
    ///
    /// The **one** place an [`EntryTables`] is built, so the driver's publication
    /// ([`CompiledModule::entry_tables`]) and an in-block transfer's re-publication
    /// ([`CompiledModule::native_entry_tables`]) can never disagree about which pointers,
    /// in which order, describe a function.
    ///
    /// [`CompiledModule::entry_tables`]: super::code::CompiledModule::entry_tables
    /// [`CompiledModule::native_entry_tables`]: super::code::CompiledModule::native_entry_tables
    pub(crate) fn of(
        func: u32,
        entries: &[Option<*const u8>],
        sub_entries: &[Option<*const u8>],
    ) -> EntryTables {
        EntryTables {
            func,
            entries: entries.as_ptr(),
            entries_len: entries.len(),
            sub_entries: sub_entries.as_ptr(),
            sub_entries_len: sub_entries.len(),
        }
    }
    /// The native entry for `(func, pc)` in these tables — a real block start first, then a
    /// continuation sub-block, exactly the precedence
    /// [`CompiledModule::entry`](super::code::CompiledModule::entry) uses.
    ///
    /// `None` when `func` is not this table's function (a different mapping, which only the
    /// driver may enter) or when the address compiled no entry.
    pub(crate) fn entry(&self, func: u32, pc: u32) -> Option<*const u8> {
        if func != self.func {
            return None;
        }
        let pc = pc as usize;
        if pc < self.entries_len {
            // SAFETY: `entries` points at the `entries_len`-slot vector of the compiled
            // function this table was published for (see the struct docs), and `pc` is in
            // bounds of it.
            if let Some(entry) = unsafe { *self.entries.add(pc) } {
                return Some(entry);
            }
        }
        if pc < self.sub_entries_len {
            // SAFETY: as above — `sub_entries` points at that function's
            // `sub_entries_len`-slot continuation table and `pc` is in bounds of it.
            return unsafe { *self.sub_entries.add(pc) };
        }
        None
    }
}
impl JitRuntime<'_> {
    /// Re-publish the current top frame's Y window into [`Self::y_base`]/[`Self::y_end`].
    ///
    /// Called from generated code — through [`jit_publish_y`] in a block's prologue and
    /// [`jit_refresh_y`] after an in-block helper call — so the emitted Y-window guard
    /// always compares against the live frame. `Machine::push_frame` grows the Erlang
    /// stack with `Vec::resize`, so a window taken before a frame push can name freed
    /// memory; re-publishing here is what keeps it current. The **driver does not call
    /// it**: every native entry point is a block prologue (a linked tail-call target is
    /// a block entry too), and the only in-block state change that can move the window
    /// is a helper call, which the emitter covers with a gated refresh — so a driver
    /// round-trip pays nothing.
    ///
    /// With no live frame both fields are null: `lea k*8; cmp; jae` then rejects every
    /// Y access (the emitted guard), so a frame-less block can never dereference one.
    ///
    /// `#[inline]`: the body is a handful of instructions and it sits on the per-block
    /// entry path, so the call overhead would otherwise be a measurable share of a cheap
    /// entry.
    #[inline]
    pub(crate) fn refresh_y_base(&mut self) {
        // SAFETY: `rt.machine` is a non-null pointer to the driver's live `Machine`;
        // no other `&mut Machine` exists while a helper/driver frame runs (the driver
        // is blocked in the block call that reached us).
        let machine = unsafe { &mut *self.machine };
        // Copy the top frame's window out *before* taking the stack pointer: the
        // `frames` borrow must end for the `stack.as_mut_ptr()` borrow to start.
        let window = machine.frames.last().map(|frame| (frame.y_base, frame.y_count));
        match window {
            Some((y_base, y_count)) => {
                // SAFETY: `push_frame` sized the stack to `y_base + y_count` and the
                // frame is still live (`deallocate`/`pop_frame` truncate the stack and
                // drop the frame together), so `[y_base, y_base + y_count]` is in
                // bounds of the current allocation. `as_mut_ptr` is freshly taken, so
                // the pointers follow any earlier reallocation.
                let base = machine.stack.as_mut_ptr();
                let start = unsafe { base.add(y_base) };
                self.y_base = start;
                self.y_end = unsafe { start.add(y_count as usize) };
            }
            None => {
                self.y_base = std::ptr::null_mut();
                self.y_end = std::ptr::null_mut();
            }
        }
    }

    /// Re-publish the Y window of a frame the caller has **just pushed**, from the
    /// `(y_base, y_count)` [`Machine::push_frame_window`] handed back.
    ///
    /// [`JitRuntime::refresh_y_base`] is the general form: it re-derives the window from
    /// `Machine::frames.last()` and a fresh `machine.stack.as_mut_ptr()`. A frame-changing
    /// helper's success arm has both figures in hand already, so it publishes through this
    /// instead — the same two stores, without the frame-stack load, the index scaling and
    /// the `y_base`/`y_count` reload that re-derivation costs.
    ///
    /// Soundness is exactly [`JitRuntime::refresh_y_base`]'s: the frame is the machine's
    /// top frame (it was just pushed) and the stack is sized to `y_base + y_count`, so the
    /// range is in bounds; the base pointer is taken *after* the push, so it follows any
    /// reallocation the push made.
    #[inline]
    pub(crate) fn publish_y_window(&mut self, y_base: usize, y_count: u32) {
        // SAFETY: `rt.machine` is a non-null pointer to the driver's live `Machine`;
        // no other `&mut Machine` exists while a helper/driver frame runs (the driver
        // is blocked in the block call that reached us).
        let machine = unsafe { &mut *self.machine };
        // SAFETY: `push_frame_window` sized the stack to `y_base + y_count`, so
        // `[y_base, y_base + y_count]` is in bounds of the current allocation; the
        // pointer is freshly taken, so it follows any earlier reallocation.
        let start = unsafe { machine.stack.as_mut_ptr().add(y_base) };
        self.y_base = start;
        self.y_end = unsafe { start.add(y_count as usize) };
    }

    /// Re-publish the literal area the machine has bound into
    /// [`Self::literals_base`]/[`Self::literals_end`].
    ///
    /// Read from the machine's **own** binding (context `0`'s entry area, else the
    /// registered context's module area) rather than from a driver-held module
    /// pointer, so the published bytes are exactly what
    /// [`Machine::literal`](crate::Machine::literal) — and therefore the
    /// interpreter's `read_operand(Operand::Literal(n))` — would yield. The two are
    /// bound together by `Machine::set_literals` (from `Machine::enter_run` at a
    /// slice start and from `Machine::switch_context`/`switch_into` at a module
    /// transfer) and are **never mutated mid-run** by anything else, so a single
    /// publish at a driver entry point stays correct for every block the driver then
    /// enters.
    ///
    /// `#[inline]`: the body is a handful of loads off the machine and it sits on the
    /// per-slice entry path, so the call overhead would otherwise be a measurable
    /// share of a cheap entry.
    #[inline]
    pub(crate) fn refresh_literals(&mut self) {
        // SAFETY: `rt.machine` is a non-null pointer to the driver's live `Machine`;
        // no other `&mut Machine` exists while this Rust-side helper runs (either the
        // driver is between blocks, or it is blocked in the block call that reached us).
        let machine = unsafe { &*self.machine };
        let (base, end) = match machine.current_context() {
            // Context `0` is the run's entry module, which the machine does not own:
            // its area is the one `Machine::enter_run` recorded.
            0 => match machine.entry_literals.as_ref() {
                Some(area) => literal_bounds(area),
                None => (std::ptr::null(), std::ptr::null()),
            },
            // A registered callee context: its own module's area, exactly what
            // `Machine::switch_context` binds.
            id => match machine.module_context(id) {
                Some(handle) => literal_bounds(&handle.module.literals),
                None => (std::ptr::null(), std::ptr::null()),
            },
        };
        self.literals_base = base;
        self.literals_end = end;
    }
}

/// The `(base, end)` pair a literal area publishes: the first literal word and one
/// past the last.
///
/// An area with no literals publishes `end == base`, which rejects **every** index
/// (the guard is `addr >= end`), so an empty area is as inert as an absent one — the
/// same discipline as a null Y window.
fn literal_bounds(area: &beam_loader::LiteralArea) -> (*const Eterm, *const Eterm) {
    let terms = area.terms();
    let base = terms.as_ptr();
    // `wrapping_add` (never `add`): a slice whose length is 0 may carry a dangling
    // (merely aligned) base, for which `add(0)` would be as valid but `wrapping_add`
    // states the intent — no dereference ever happens through these pointers unless
    // the emitted guard has already proven `base + n*8 < end`.
    (base, base.wrapping_add(terms.len()))
}
/// A compiled block's native signature.
pub(crate) type NativeBlock = for<'a> extern "C" fn(*mut JitRuntime<'a>) -> i64;

/// The `jit_step_one` helper's native signature (the block ABI plus the
/// instruction address).
pub(crate) type StepHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32) -> i64;

/// The `jit_tail_call` helper's native signature (the block ABI plus the resolved
/// successor word and the frame size a local `call_last` pops).
pub(crate) type TailHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, i64, u32) -> i64;

/// The `jit_local_call` helper's native signature (the block ABI plus the instruction's
/// own `(func, pc)`, the [`encode_local_call`] word naming the emitter's resolved callee
/// and its register-argument count, the [`LOCAL_CALL_ONLY`]/[`LOCAL_CALL_LAST`] kind and
/// the `call_last` frame size).
pub(crate) type LocalCallHelper =
    for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, i64, u32, u32) -> i64;

/// The `jit_deallocate`/`jit_deallocate_return` helpers' native signature (the block ABI
/// plus the instruction's own `(func, pc)` and the frame size the checked pop must match).
pub(crate) type FrameExitHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32) -> i64;

/// The `jit_return` helper's native signature (the block ABI plus the instruction's own
/// `(func, pc)`); a bare `return` pops nothing, so it carries no frame size.
pub(crate) type ReturnHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32) -> i64;

/// The `jit_allocate`/`jit_trim` helpers' native signature (the block ABI plus the
/// instruction's own `(func, pc)` and the single frame-size word the instruction
/// carries — the `allocate` frame's Y-slot count, or the `trim`'s pop count).
pub(crate) type FrameSizeHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32) -> i64;

/// The `jit_test_heap` helper's native signature (the block ABI plus the instruction's
/// own `(func, pc)`, the heap-word need and the live x-register count).
pub(crate) type HeapNeedHelper =
    for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32, u32) -> i64;

/// The `jit_allocate_heap` helper's native signature (the block ABI plus the
/// instruction's own `(func, pc)`, the frame's Y-slot count, the heap-word need and the
/// live x-register count).
pub(crate) type AllocateHeapHelper =
    for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32, u32, u32) -> i64;

/// The `jit_ext_call` helper's native signature (the block ABI plus the
/// instruction's own `(func, pc)`, the `ImportRef` index, the [`EXT_CALL`]-family
/// kind and — for `call_ext_last` — the frame size to pop).
pub(crate) type ExtCallHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32, u32, u32) -> i64;
/// The [`jit_publish_y`] helper's native signature (the block ABI plus the block's own
/// `(func, pc)`); it returns nothing — a position store and a window re-derivation
/// cannot fail.
///
/// The **only** user of this shape now: the plain position store is emitted inline
/// (`mov r10,[rdi+RUNTIME_CURRENT_CODE_DISP]`; `mov r11, movabs pack_current_code(..)`;
/// `mov [r10], r11`), because both its operands are compile-time constants and a helper
/// call was pure overhead. The Y publish stays a call because it must re-derive the
/// window from live `Machine` state.
pub(crate) type PositionHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32);

/// The `jit_refresh_y` helper's native signature (the block ABI only); it returns
/// nothing — re-publishing the frame's Y window cannot fail.
pub(crate) type RefreshHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>);

/// The byte offset of [`JitRuntime::machine`] — the field the helper scaffolding is
/// handed; generated code reads [`JitRuntime::x_regs`] instead.
pub(crate) const RUNTIME_MACHINE_DISP: i32 = 0;

/// The byte offset of [`JitRuntime::x_regs`] (the base of `machine.x`) — the first
/// field generated code loads indirectly. It is the second `#[repr(C)]` pointer
/// field, hence one pointer width past `machine`.
pub(crate) const RUNTIME_XREGS_DISP: i32 =
    RUNTIME_MACHINE_DISP + mem::size_of::<*const u8>() as i32;

/// The byte offset of [`JitRuntime::y_base`] (the base of the current frame's first Y
/// slot) — a field generated code loads indirectly for a Y-operand or Y-destination
/// arithmetic fast path.
pub(crate) const RUNTIME_YBASE_DISP: i32 = RUNTIME_XREGS_DISP + mem::size_of::<*const u8>() as i32;

/// The byte offset of [`JitRuntime::y_end`] (one past the current frame's last Y slot)
/// — the bound the emitted Y-window guard compares a Y slot's address against.
pub(crate) const RUNTIME_YEND_DISP: i32 = RUNTIME_YBASE_DISP + mem::size_of::<*const u8>() as i32;

/// The byte offset of [`JitRuntime::literals_base`] (the base of the bound literal
/// area) — a field generated code loads indirectly for an `Operand::Literal(n)`
/// `move`'s source word.
///
/// Computed with [`mem::offset_of!`] rather than by arithmetic on the preceding field:
/// `error: Option<BeamError>` and `enter: Option<(u32, u32, u32)>` sit between it and
/// the Y window, and neither has a size the emitter should have to know.
pub(crate) const RUNTIME_LITERALS_DISP: i32 =
    mem::offset_of!(JitRuntime<'static>, literals_base) as i32;

/// The byte offset of [`JitRuntime::literals_end`] (one past the bound literal area's
/// last word) — the bound the emitted literal guard compares a literal's address
/// against, and the reason an absent area rejects every index.
pub(crate) const RUNTIME_LITERALS_END_DISP: i32 =
    RUNTIME_LITERALS_DISP + mem::size_of::<*const Eterm>() as i32;

/// The byte offset of [`JitRuntime::resume_entry`] — the same-function block entry a
/// `return` helper resolved, which the emitted return arm tail-jumps to instead of leaving
/// the block with a word for the driver.
///
/// Null means "no in-block continuation", so the arm keeps today's behaviour and returns
/// the helper's word. It follows [`JitRuntime::literals_end`] directly (both are pointer
/// fields at the end of the struct, which only Rust reads besides this one load); computed
/// with [`mem::offset_of!`] so the constant cannot drift from the field's real position.
pub(crate) const RUNTIME_RESUME_DISP: i32 =
    mem::offset_of!(JitRuntime<'static>, resume_entry) as i32;

/// The byte offset of [`JitRuntime::current_code`] — the address of
/// `Machine::current_code`, the one word a block's prologue stores its own `(func, pc)`
/// into.
///
/// The block prologue's position store used to be a call to a `jit_set_position` helper, which
/// cost the `rt` slot load, two `movabs` argument loads, a `movabs` of the helper address
/// and the call — for a destination that is one `u64` written with two compile-time
/// constants. The emitted sequence now loads this field (`mov r10,[rdi+DISP]`), moves the
/// packed word into a scratch register and stores it, so a block entry pays three
/// instructions instead of a call. Computed with [`mem::offset_of!`] so the constant
/// cannot drift from the field's real position (the field is **appended** so no earlier
/// `RUNTIME_*_DISP` offset moves).
pub(crate) const RUNTIME_CURRENT_CODE_DISP: i32 =
    mem::offset_of!(JitRuntime<'static>, current_code) as i32;
// ---------------------------------------------------------------------------
// The helper
// ---------------------------------------------------------------------------

/// Fold a [`Step`] (or a propagating raise) into the block-return word — the **one**
/// definition of the block-ABI encoding, shared by every helper that runs
/// interpreter semantics ([`jit_step_one`], [`jit_step_ext`]).
///
/// `func`/`pc` are the address of the instruction the helper ran; they are used only
/// for [`Step::Next`], which advances to `pc + 1`.
///
/// This is the *boxed*-error form: a caller already holding a `Result` with a
/// [`BeamError`] payload costs a heap allocation for the box on its `Err` arm and a
/// footprint the size of [`BeamError`] (72 bytes) for the whole `Result` — which is
/// why a helper whose success path is the rare-path-free steady state calls
/// [`fold_ok_step`]/[`fold_err`] directly instead (see [`super::jit_family`]).
pub(crate) fn fold_step(
    runtime: &mut JitRuntime<'_>,
    step: std::result::Result<Step, Box<BeamError>>,
    func: u32,
    pc: u32,
) -> i64 {
    match step {
        Ok(step) => fold_ok_step(runtime, step, func, pc),
        Err(err) => fold_err(runtime, *err),
    }
}

/// Fold a **successful** [`Step`] into the block-return word — the success half of
/// [`fold_step`], reachable without building a `Result` over [`BeamError`].
///
/// A helper whose body already computes the family's own `Step` (the steady state of
/// every family-owned instruction) matches on that body's `Result` once, at its own
/// call site, and reaches this function with a plain [`Step`] — so no 72-byte error
/// union is ever materialised, copied or boxed on the path that succeeds.
pub(crate) fn fold_ok_step(runtime: &mut JitRuntime<'_>, step: Step, func: u32, pc: u32) -> i64 {
    match step {
        Step::Next => encode_target(func, pc + 1),
        Step::Goto { func, pc } => encode_target(func, pc),
        // Hand the resume address to the driver (see `WAIT_BIT`): a blocking-aware
        // driver reports it as `RunOutcome::Wait`, a non-blocking one folds it into
        // the typed `blocked` error (never a wrong result and never a silent
        // continue at the `wait`'s own re-examination label).
        Step::Wait { func, pc } => encode_wait(func, pc),
        // A BIF answered `BifOutcome::Suspend`: the instruction at `(func, pc)` was NOT
        // applied, and the driver must report it (as `RunOutcome::Suspend`) so the host
        // can resume by re-executing it. A word carries no address beside its sentinel,
        // so the address rides the runtime's side channel — the [`Step::Enter`]
        // precedent below.
        Step::Suspend { func, pc } => {
            runtime.suspend = Some((func, pc));
            SUSPEND
        }
        // A module-context transfer (a `call_ext*` the host resolved through
        // `ExecutionContext::resolve_external_call`, a `return` onto a
        // caller-context continuation, or a raise landing in another context's catch
        // frame). A compiled block's code, label map and literal area are all bound
        // for one module, so the block cannot make the switch itself: it hands the
        // resolved target to the driver, which owns the module-context registry
        // (`Machine::switch_context`) exactly as the interpreter's driver does.
        Step::Enter {
            context,
            func,
            pc,
        } => {
            runtime.enter = Some((context, func, pc));
            ENTER
        }
        Step::Done => HALT,
    }
}
/// Fold a **propagating raise** into the block-return word: park it on the runtime for
/// the driver, and report [`ERROR`] — the `Err` half of [`fold_step`].
///
/// Takes the [`BeamError`] by value, so a helper that already owns the error (the
/// family helper's `Err` arm, after any raise-funnel decoration) hands it over with no
/// allocation: the box exists only where a caller has already built a
/// `Result<_, Box<BeamError>>`.
pub(crate) fn fold_err(runtime: &mut JitRuntime<'_>, err: BeamError) -> i64 {
    runtime.error = Some(err);
    ERROR
}

/// The per-instruction deopt: execute exactly one instruction with the
/// interpreter's own semantics.
///
/// This is the block ABI's helper. A block calls it (with `rt`, the instruction's
/// `func` and `pc`) for the first instruction it could not lower natively; the
/// helper runs [`Machine::step_instruction`](crate::Machine) — *the* interpreter
/// step — and maps its result onto the same return word the block ABI uses
/// ([`fold_step`], the one definition of that encoding):
///
/// * [`Step::Next`] → `encode_target(func, pc + 1)`,
/// * [`Step::Goto { func, pc }`] → `encode_target(func, pc)`,
/// * [`Step::Wait { func, pc }`] → [`encode_wait`]: the resume address is handed to
///   the driver instead of being folded here, so a blocking-aware driver can report
///   it as [`crate::RunOutcome::Wait`] and a **non-blocking** one folds it into the
///   typed [`crate::error::blocked`] error (a `receive` that finds no message cannot
///   suspend such an entry),
/// * [`Step::Suspend { func, pc }`] → [`SUSPEND`] with the address parked in
///   [`JitRuntime::suspend`]: the instruction was **not** applied, so the driver
///   reports [`crate::RunOutcome::Suspend`] and the host's resume re-executes it (a
///   non-blocking entry folds it into [`crate::error::suspended`]),/// * [`Step::Done`] → [`HALT`],
/// * `Err(error)` → the error is stored in `rt.error` and [`ERROR`] is returned.
pub(crate) extern "C" fn jit_step_one(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer
    // it holds points at an object the driver owns and keeps alive for the whole
    // loop (`machine`, `module`), or at a local (`labels`) that outlives the loop.
    let runtime = unsafe { &mut *rt };
    // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`;
    // no other `&mut Machine` exists while the helper runs (the driver is blocked in
    // the block call that reached us).
    let machine = unsafe { &mut *runtime.machine };
    // SAFETY: `runtime.module` points at the `&Module` the driver is running, which
    // outlives the loop.
    let module = unsafe { &*runtime.module };
    // SAFETY: `runtime.entry_module` points at the run's entry module, which the
    // driver holds alive for the whole loop (the caller's entry module).
    let entry_module = unsafe { &*runtime.entry_module };
    // SAFETY: `runtime.labels` points at the driver's `labels` local, which outlives
    // the loop.
    let labels = unsafe { &*runtime.labels };
    // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
    // a raw pointer; the driver holds no `&mut` to it while the helper runs.
    let ctx = unsafe { &mut *runtime.ctx };

    let step = machine.step_instruction_with_entry(entry_module, module, labels, func, pc, ctx);
    // Structural counter: attribute this per-instruction deopt to the opcode at
    // `(func, pc)` in the active module.
    #[cfg(test)]
    super::counters::record_step_one(module, func, pc);
    fold_step(runtime, step, func, pc)
}

/// The `call_ext*` **decline** step: run the `call_ext*` instruction at `(func, pc)`
/// through [`Machine::call_ext_dispatch`] with the host's **already-computed** seam
/// answer, so the host's cross-module seam is consulted **once** per declined call
/// instead of twice.
///
/// A native tier lowers a `call_ext*` through the [`jit_ext_call`] helper, which
/// consults the host seam itself ([`ExecutionContext::resolve_external_call`]). When
/// the host answers "run it synchronously in this process" (`Ok(None)` — the common
/// BIF case) the helper used to hand the *whole* instruction back to
/// [`jit_step_one`], and the interpreter's `call_ext*` arm then resolved the **same**
/// import through the **same** seam a second time ([`Machine::resolve_external_entry`]
/// inside [`Machine::call_ext_dispatch`]) before falling through to the synchronous
/// `call_bif` route. `seam` carries the host's answer verbatim (`Some(None)` = "the
/// host declined"), which [`Machine::call_ext_dispatch`] consumes **without** asking
/// the seam again, while still running the interpreter's own `erlang:apply/3` /
/// `erlang:send/2` special cases and the variant's frame discipline — so this is the
/// interpreter's `call_ext*` semantics with the duplicate resolution removed.
///
/// The instruction's own operand shape (`arity`, and the variant's `deallocate`) is
/// read straight out of the active module's code, so the helper reproduces the exact
/// [`ImportFrame`] the interpreter's `call_ext*` arm builds. Everything else — the
/// `Step` → block-ABI folding, the position store, the context-`0` half of the raise
/// funnel — is [`jit_step_one`]'s, so the two paths are interpreter-identical by
/// construction.
///
/// Deliberately **not** `extern "C"`/in the helper fn-pointer table: it is reachable
/// only from [`jit_ext_call`] (Rust), never directly from generated code, so its
/// `seam` argument may be an ordinary Rust `Option`.
fn jit_step_ext(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    seam: Option<Option<(u32, u32, u32)>>,
) -> i64 {
    let step = {
        // SAFETY: the driver passes `rt` as the address of a live, exclusively
        // borrowed `JitRuntime` and never aliases it for the duration of the call;
        // every pointer it holds points at an object the driver owns and keeps alive
        // for the whole loop (`machine`, `module`), or at a local (`labels`) that
        // outlives the loop.
        let runtime = unsafe { &mut *rt };
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.module` points at the module the driver is running, which
        // outlives the loop.
        let module = unsafe { &*runtime.module };
        // SAFETY: `runtime.entry_module` points at the run's entry module, which the
        // driver holds alive for the whole loop.
        let entry_module = unsafe { &*runtime.entry_module };
        // SAFETY: `runtime.labels` points at the driver's `labels` local, which
        // outlives the loop.
        let labels = unsafe { &*runtime.labels };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed
        // as a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        // The same position store `Machine::step_instruction` makes at entry, so
        // `Machine::current_code` keeps the granularity this instruction had before the
        // duplicate resolution was removed.
        machine.set_current_code(func, pc);
        // Structural counter: attribute this `call_ext*` deopt to the opcode at
        // `(func, pc)` in the active module.
        #[cfg(test)]
        super::counters::record_step_ext(module, func, pc);

        // The instruction's own operand shape selects the frame discipline, exactly as
        // the interpreter's three `call_ext*` arms do (`ImportFrame::Call`/`Only`/
        // `Last(deallocate)`).
        let step = match module
            .functions
            .get(func as usize)
            .and_then(|function| function.code.get(pc as usize))
        {
            Some(Instr::CallExt { arity, target }) => machine.call_ext_dispatch(
                entry_module,
                module,
                labels,
                *target,
                *arity,
                func,
                pc,
                ImportFrame::Call,
                ctx,
                seam,
            ),
            Some(Instr::CallExtOnly { arity, target }) => machine.call_ext_dispatch(
                entry_module,
                module,
                labels,
                *target,
                *arity,
                func,
                pc,
                ImportFrame::Only,
                ctx,
                seam,
            ),
            Some(Instr::CallExtLast {
                arity,
                target,
                deallocate,
            }) => machine.call_ext_dispatch(
                entry_module,
                module,
                labels,
                *target,
                *arity,
                func,
                pc,
                ImportFrame::Last(*deallocate),
                ctx,
                seam,
            ),
            // Not a `call_ext*`: `jit_ext_call` only reaches this helper for one, so
            // this defensive arm takes the interpreter's full step (exactly what the
            // helper's old fallback did) rather than inventing a result.
            _ => machine.step_instruction_with_entry(entry_module, module, labels, func, pc, ctx),
        };
        // The uniform raise funnel's context-`0` half, exactly as
        // `Machine::step_instruction` runs it: in context `0` the active module IS the
        // entry module, so a propagating raise is decorated here; a non-zero context is
        // left to the driver, which knows the true entry module.
        if machine.current_context() == 0 {
            step.map_err(|err| Box::new(machine.with_uncaught_frames(*err, Some(module), func, pc)))
        } else {
            step
        }
    };
    // SAFETY: `rt` is the same live, exclusively borrowed `JitRuntime` the driver
    // owns; the `&mut` derived from it above is dead by now, so this borrow does not
    // alias it.
    let runtime = unsafe { &mut *rt };
    fold_step(runtime, step, func, pc)
}

/// The kind of `call_ext*` a [`jit_ext_call`] call describes: `call_ext Arity Bif` —
/// the caller's frame is kept and the return address stored in it.
pub(crate) const EXT_CALL: u32 = 0;

/// The kind of `call_ext*` a [`jit_ext_call`] call describes: `call_ext_only Arity
/// Bif` — a tail call, so the caller's frame is reused unchanged.
pub(crate) const EXT_CALL_ONLY: u32 = 1;

/// The kind of `call_ext*` a [`jit_ext_call`] call describes: `call_ext_last Arity
/// Bif Deallocate` — the caller's frame is popped before the callee is entered.
pub(crate) const EXT_CALL_LAST: u32 = 2;

/// The **local-call seam's decline word**: [`jit_local_call`] emits a *local*
/// `call_only`/`call_last` as a run-time seam probe whose decline arm must fall
/// through to the block's **static** link, so the helper needs a word that is neither
/// a real continuation nor one of the block ABI's other sentinels.
///
/// It is compared (and consumed) entirely **inside the emitted block** — `cmp rax,
/// LOCAL_DECLINE; jcc Ne, done` — so it never reaches the driver: the only word a
/// block returns for a probed local call is the static exit's own (a continuation
/// word, or one of [`HALT`]/[`ERROR`]/[`ENTER`]/the [`WAIT_BIT`] encoding), exactly as
/// before the probe existed.
pub const LOCAL_DECLINE: i64 = -4;

/// The kind of local tail call a [`jit_local_call`] call describes: `call_only Arity
/// Label` — a tail call with no frame to pop.
pub(crate) const LOCAL_CALL_ONLY: u32 = 0;

/// The kind of local tail call a [`jit_local_call`] call describes: `call_last Arity
/// Label Deallocate` — a tail call that pops the `deallocate` frame.
pub(crate) const LOCAL_CALL_LAST: u32 = 1;

/// Packs a local call's resolved callee and its register-argument count into the one
/// block-ABI word [`jit_local_call`] receives.
///
/// Both are 32-bit (`func` is a `Module::functions` index, `arity` a `u32` operand), so
/// the packing is lossless — the same two-u32-in-one-i64 shape [`encode_target`] uses to
/// carry a `(func, pc)` pair through the block ABI. It exists to keep the helper inside
/// the six integer argument registers (`rt` plus five) [`emit_helper_call`] can fill.
pub(crate) const fn encode_local_call(callee: u32, arity: u32) -> i64 {
    ((callee as i64) << 32) | (arity as i64 & 0xFFFF_FFFF)
}

/// Decodes an [`encode_local_call`] word back into its `(callee, arity)` pair.
pub(crate) const fn decode_local_call(word: i64) -> (u32, u32) {
    ((word >> 32) as u32, (word & 0xFFFF_FFFF) as u32)
}

/// The **`call_ext*`** helper: resolve the import through the host's cross-module-call
/// seam and, when it answers, apply the variant's frame discipline and hand the
/// callee to the driver as the [`ENTER`] word — the same transfer [`jit_step_one`]
/// produces for a `Step::Enter`, without paying an interpreter step for the call
/// instruction itself.
///
/// `kind` is one of [`EXT_CALL`]/[`EXT_CALL_ONLY`]/[`EXT_CALL_LAST`] and `deallocate`
/// the `call_ext_last` frame size (ignored otherwise). The per-variant discipline is
/// exactly the interpreter's ([`crate::interpreter`]'s `Instr::CallExt*` arms):
///
/// * `CallExt` stores `Continuation::Address { func, pc + 1, context: current }` in
///   the live frame (an `Instr::Call`);
/// * `CallExtOnly` stores nothing (an `Instr::CallOnly`);
/// * `CallExtLast` pops the frame with [`Machine::pop_frame_checked`] — the
///   interpreter's own checked pop — **after** the target resolved, so a bad label
///   can never leave a popped frame behind (an `Instr::CallLast`).
///
/// Anything the host cannot run in this machine — the seam's `Ok(None)` (the
/// synchronous `call_bif` path, NIFs, the runtime's native floor), an import whose
/// atoms the module's table lacks, a target label the callee does not define, a
/// frame discipline that failed — keeps the synchronous path byte-identical to the
/// tier before this arm existed. Two routes get there:
///
/// * the host's **decline** (`Ok(None)`) goes through [`jit_step_ext`]'s
///   `Some(None)` seam answer, which drives the instruction through
///   [`Machine::call_ext_dispatch`] **without** re-asking the host's seam;
/// * every *other* unresolved shape re-runs the whole instruction through
///   [`jit_step_one`], the interpreter step that re-resolves and reports its own
///   exact diagnostic. Nothing is mutated before that fallback, so the interpreter
///   sees exactly the state it would have seen.
///
/// The import's `(module name, function name, arity)` is read straight out of the
/// active module's `ImpT` table and atom table — the same triple the per-run import
/// cache ([`LabelMap`]'s `import_target`) answers with, including its three typed
/// errors — and `arity` is the import's **declared** arity, which is what the
/// interpreter hands the seam.
pub(crate) extern "C" fn jit_ext_call(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    target: u32,
    kind: u32,
    deallocate: u32,
) -> i64 {
    let resolved = {
        // SAFETY: the driver passes `rt` as the address of a live, exclusively
        // borrowed `JitRuntime` and never aliases it for the duration of the call;
        // every pointer it holds points at an object the driver owns and keeps alive
        // for the whole loop (`machine`, `module`), or at a local (`labels`) that
        // outlives the loop.
        let runtime = unsafe { &mut *rt };
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.module` points at the module the driver is running, which
        // outlives the loop.
        let module = unsafe { &*runtime.module };
        // SAFETY: `runtime.labels` points at the driver's `labels` local, which
        // outlives the loop.
        let labels = unsafe { &*runtime.labels };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed
        // as a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        resolve_ext_entry(
            machine,
            module,
            labels,
            ImportRef(target),
            kind,
            func,
            pc,
            deallocate,
            ctx,
        )
    };
    match resolved {
        Some(Some((context, callee, callee_pc))) => {
            // Structural counter: the `call_ext*` resolved a callee for this machine.
            #[cfg(test)]
            super::counters::record_ext_resolved();
            // SAFETY: `rt` is the same live, exclusively borrowed `JitRuntime` the
            // driver owns; the `&mut` derived from it above is dead by now, so this
            // store does not alias it. The driver takes the slot when it decodes the
            // `ENTER` word.
            unsafe { (*rt).enter = Some((context, callee, callee_pc)) };
            ENTER
        }
        // The host declined **this** import (`Ok(None)`): hand that answer to the
        // dispatching seam so the instruction runs with the interpreter's own
        // `call_ext*` semantics — special cases and frame discipline included — while
        // the host's seam is consulted only once.
        Some(None) => {
            #[cfg(test)]
            super::counters::record_ext_declined();
            jit_step_ext(rt, func, pc, Some(None))
        }
        // Resolution never reached the host's decline, so the instruction is the
        // interpreter's to execute (all of it, exactly once).
        None => {
            #[cfg(test)]
            super::counters::record_ext_fallback();
            jit_step_one(rt, func, pc)
        }
    }
}

/// Resolve a `call_ext*` through the host's seam and apply its frame discipline.
///
/// `None` means "the interpreter step must re-run this instruction": the import does
/// not resolve, the host's seam **errored** (not a decline), the callee's entry label
/// is not in its own label index, or the frame discipline failed. **Nothing has been
/// mutated** on any of those paths (registering a module context is idempotent, the
/// continuation store and the frame pop both validate before writing), so the caller's
/// [`jit_step_one`] fallback re-runs the instruction and produces exactly what the
/// interpreter would have produced.
///
/// `Some(None)` means the host **declined** the import (`Ok(None)`): the caller can
/// answer the seam's own question verbatim ([`jit_step_ext`] with `Some(None)`), so
/// [`Machine::call_ext_dispatch`] never re-asks it. `Some(Some(target))` is a resolved
/// callee this machine can enter.
fn resolve_ext_entry(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    target: ImportRef,
    kind: u32,
    func: u32,
    pc: u32,
    deallocate: u32,
    ctx: &mut dyn ExecutionContext,
) -> Option<Option<(u32, u32, u32)>> {
    // The import's `(module, function, arity)`, through the same per-run import cache
    // the interpreter's own `call_ext*` arm uses (`LabelMap::import_target`, which falls
    // back to the uncached resolution when the map was not built from this module, so
    // the two can never disagree). An index past the end of `ImpT` and an atom the
    // module's table does not hold are not resolvable, so the interpreter must report
    // them — the fallback re-runs the instruction and produces its exact diagnostic.
    let (module_name, function_name, arity) = match labels.import_target(module, target) {
        Ok(triple) => triple,
        Err(_) => return None,
    };
    let (callee, label) = match ctx.resolve_external_call(module_name, function_name, arity) {
        // The host's **decline**: report the seam's own answer so the caller can skip
        // the duplicate resolution (see `jit_step_ext`).
        Ok(None) => return Some(None),
        Ok(Some(resolved)) => resolved,
        // A host *error* is not a decline: leave the instruction to the interpreter,
        // which re-asks the seam and reports that error exactly as it always has.
        Err(_) => return None,
    };
    // Registering is idempotent by the handle's allocation identity, so a recursive or
    // repeated cross-module call reuses one context (and its already-built label index).
    let context = machine.register_context(&callee);
    let handle = machine.module_context(context)?;
    // The entry label is resolved against the **callee's own** label index, so an
    // unknown label is never a jump into the wrong module.
    let (callee_func, callee_pc) = handle.labels.resolve(label).ok()?;
    let discipline = match kind {
        EXT_CALL => {
            // The caller's context, read *before* the store (the callee's context is
            // the one being entered): a `return` in the callee comes back here.
            let caller_context = machine.current_context();
            machine.set_continuation(Continuation::Address {
                func,
                pc: pc + 1,
                context: caller_context,
            })
        }
        EXT_CALL_ONLY => Ok(()),
        EXT_CALL_LAST => machine.pop_frame_checked(deallocate),
        _ => Err(error::unsupported("jit: unknown call_ext kind")),
    };
    discipline.ok()?;
    Some(Some((context, callee_func, callee_pc)))
}

/// The **local-call seam** helper: consult the host's
/// [`ExecutionContext::resolve_local_call`] for a same-module `call_only`/`call_last`
/// target and, when the host serves it, execute the arm's synchronous BIF route — the
/// native tier's half of the seam the interpreter's three local call arms honour (see
/// [`Machine::local_call_seam`], the **one** definition of that behaviour the two tiers
/// share).
///
/// The instruction's own operand shape selects the frame discipline and the register
/// arity, exactly as the interpreter's arms do ([`ImportFrame::Only`] for a
/// `call_only`, [`ImportFrame::Last`] with its `deallocate` for a `call_last`); a
/// plain `call` is lowered by its own helper ([`jit_call_store`]) and consults the
/// same seam there, so this probe serves only the `call_only`/`call_last` arms.
///
/// The instruction's whole compile-time resolution — the caller's `(func, pc)`, the
/// callee's function index, its register-argument count, the kind and (for a
/// `call_last`) the frame size — arrives as **block-ABI immediates**, exactly like
/// [`jit_ext_call`]'s. The emitter resolves the label for its own transfer anyway, so
/// the helper never re-reads `module.functions[func].code[pc]` and never re-resolves a
/// label per call; a debug build re-derives the pair from the instruction and the
/// driver's label map to prove the two agree (see `probe_local_call`), while a release
/// build trusts the emitter, which is the single source of truth.
///
/// Three outcomes, all of them the block ABI's own vocabulary:
///
/// * the folded [`Step`] of a served local call (a tail call always yields a
///   `Goto`/[`Step::Done`]/[`Step::Enter`], never a `Next`), including the landed raise
///   a failing BIF produced (`ERROR` with `rt.error` set);
/// * [`LOCAL_DECLINE`] when the host declined (`Ok(false)`) or the callee's identity
///   cannot be named. **Nothing is mutated**
///   on this path — the host seam is a `&mut ExecutionContext`, which cannot reach
///   `machine.x` — so the emitted block's unchanged static exit (the direct `jmp`, or
///   `jit_tail_call`'s checked pop plus its `jmp`) sees exactly the state it saw before
///   the probe existed.
pub(crate) extern "C" fn jit_local_call(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    callee_arity: i64,
    kind: u32,
    deallocate: u32,
) -> i64 {
    let outcome = {
        // SAFETY: the driver passes `rt` as the address of a live, exclusively
        // borrowed `JitRuntime` and never aliases it for the duration of the call;
        // every pointer it holds points at an object the driver owns and keeps alive
        // for the whole loop (`machine`, `module`), or at a local (`labels`) that
        // outlives the loop.
        let runtime = unsafe { &mut *rt };
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.module` points at the module the driver is running, which
        // outlives the loop.
        let module = unsafe { &*runtime.module };
        // SAFETY: `runtime.entry_module` points at the run's entry module, which the
        // driver holds alive for the whole loop.
        let entry_module = unsafe { &*runtime.entry_module };
        // SAFETY: `runtime.labels` points at the driver's `labels` local, which
        // outlives the loop.
        let labels = unsafe { &*runtime.labels };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed
        // as a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };
        probe_local_call(
            machine,
            entry_module,
            module,
            labels,
            func,
            pc,
            callee_arity,
            kind,
            deallocate,
            ctx,
        )
    };
    match outcome {
        None => LOCAL_DECLINE,
        Some(step) => {
            // SAFETY: `rt` is the same live, exclusively borrowed `JitRuntime` the
            // driver owns; the `&mut` derived from it above is dead by now, so this
            // borrow does not alias it.
            let runtime = unsafe { &mut *rt };
            fold_step(runtime, step, func, pc)
        }
    }
}

/// The Rust body of [`jit_local_call`]: build the arm's [`ImportFrame`] from the
/// emitter's kind/size pair, consult the host seam through
/// [`Machine::local_call_seam`], and report either the arm's outcome or `None` for a
/// **decline** (today's static link).
///
/// Separate from the `extern "C"` helper so it may take ordinary Rust types (the
/// `Option`, the `&Module` both come from the driver's live objects) and so the
/// exit path's borrows are provably dead before [`fold_step`] takes the runtime again
/// — the same split [`jit_step_ext`] uses.
#[allow(clippy::too_many_arguments)]
fn probe_local_call(
    machine: &mut Machine,
    entry_module: &Module,
    module: &Module,
    labels: &LabelMap,
    func: u32,
    pc: u32,
    callee_arity: i64,
    kind: u32,
    deallocate: u32,
    ctx: &mut dyn ExecutionContext,
) -> Option<std::result::Result<Step, Box<BeamError>>> {
    let (callee, arity) = decode_local_call(callee_arity);
    let frame = match kind {
        LOCAL_CALL_ONLY => ImportFrame::Only,
        LOCAL_CALL_LAST => ImportFrame::Last(deallocate),
        // Not a kind this probe is emitted for: declining leaves the instruction on the
        // block's own static exit, which is all the probe was ever a preface to.
        _ => return None,
    };
    // The emitted immediates ARE the interpreter arm's step-1 `resolve` result, so a
    // release build has nothing left to derive. A debug build proves it: the address
    // names this very tail call, its `arity` operand is the one handed to the seam, the
    // emitter's kind names the variant, and the callee is the resolution of the
    // instruction's own target label. A `debug_assert!`, never a run-time branch — the
    // shipped path pays nothing for it.
    debug_assert!(
        match module
            .functions
            .get(func as usize)
            .and_then(|function| function.code.get(pc as usize))
        {
            Some(Instr::CallOnly { target, arity: operand }) =>
                *operand == arity
                    && kind == LOCAL_CALL_ONLY
                    && matches!(labels.resolve(*target), Ok((resolved, _)) if resolved == callee),
            Some(Instr::CallLast {
                target,
                arity: operand,
                deallocate: size,
            }) =>
                *operand == arity
                    && kind == LOCAL_CALL_LAST
                    && *size == deallocate
                    && matches!(labels.resolve(*target), Ok((resolved, _)) if resolved == callee),
            _ => false,
        },
        "jit_local_call: the emitter passes the tail call's own resolution for (func={func}, pc={pc})"
    );
    #[cfg(not(debug_assertions))]
    let _ = labels;
    let step = match machine.local_call_seam(
        entry_module,
        module,
        callee,
        arity,
        frame,
        func,
        pc,
        ctx,
    ) {
        Ok(None) => return None,
        Ok(Some(step)) => Ok(step),
        Err(err) => Err(err),
    };
    Some(decorate(machine, module, step, func, pc))
}

/// Apply the uniform raise funnel's **entry-context** half to a probed local call's
/// outcome — exactly what `Machine::step_instruction` does around an instruction it
/// executes: in the run's entry context (`0`) the active module IS the entry module,
/// so a propagating raise is decorated here; a non-zero context is left to the
/// driver, which knows the true entry module. The driver's `ERROR` path returns
/// `rt.error` without re-running the funnel, so decorating here is what makes a
/// probed raise carry the same frames as the interpreter's.
fn decorate(
    machine: &mut Machine,
    module: &Module,
    step: std::result::Result<Step, Box<BeamError>>,
    func: u32,
    pc: u32,
) -> std::result::Result<Step, Box<BeamError>> {
    if machine.current_context() == 0 {
        step.map_err(|err| Box::new(machine.with_uncaught_frames(*err, Some(module), func, pc)))
    } else {
        step
    }
}

/// The **Y-window publish** helper: re-publish the current frame's Y window *and*
/// record the block's entry position, in one call.
///
/// It is the prologue's position store ([`JitRuntime::current_code`], written inline by
/// generated code — `emit::emit_position_store`) plus [`JitRuntime::refresh_y_base`], and
/// a block **whose body accesses a Y slot** calls it from its prologue *in place of* the
/// inline store — so such a block pays no extra call for publishing its window, and a
/// block that never touches a Y slot keeps the three-instruction inline store (it pays
/// nothing).
///
/// The prologue is the cheapest sound publish point: **every** native entry runs one —
/// a fresh driver entry, a linked branch, a linked tail call and a continuation
/// sub-block all start at a block prologue — so publishing here covers a frame a helper
/// moved before the block was entered (an `allocate`'s `push_frame`, a `deallocate`'s
/// pop, or the `Vec` reallocation a push may cause), without a per-round-trip refresh in
/// the driver. The one in-block state change the prologue does not cover is a helper
/// call the block then continues past; the emitter covers it with a refresh
/// ([`jit_refresh_y`]) gated to a block that still accesses a Y slot after that point.
///
/// Infallible, and it clobbers only caller-saved registers, so it is emitted before the
/// prologue loads the cached x-registers (the same reason the inline position store is).
pub(crate) extern "C" fn jit_publish_y(rt: *mut JitRuntime<'_>, func: u32, pc: u32) {
    // Structural counter: one block prologue whose body accesses a `Y` slot.
    #[cfg(test)]
    super::counters::record_publish_y();
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; `runtime.machine`
    // points at the driver's live `Machine`, and no other `&mut Machine` exists while
    // the helper runs (the driver is blocked in the block call that reached us).
    let runtime = unsafe { &mut *rt };
    runtime.refresh_y_base();
    // SAFETY: see above — `runtime.machine` is a non-null pointer to the driver's live
    // `Machine`; the `&mut JitRuntime` above is dead by now, so borrowing through the
    // field does not alias it.
    let machine = unsafe { &mut *runtime.machine };
    machine.set_current_code(func, pc);
}

/// The **Y-window refresh** helper: re-publish the current frame's Y window after an
/// in-block helper call, so the next Y access in the block compares against the live
/// frame.
///
/// A block emits this on the *continue* arm of an in-block helper call only when a later
/// instruction of the block still accesses a Y slot (the emitter's gated refresh): the
/// helper ran one instruction in the interpreter, which may have pushed a frame,
/// `trim`med one, or **reallocated** `machine.stack` — any of which moves or frees the
/// window the block published in its prologue. When no later instruction accesses a Y
/// slot the call is not emitted at all, and when the block leaves to the driver the
/// next block's prologue publishes instead (so a driver round-trip never refreshes).
pub(crate) extern "C" fn jit_refresh_y(rt: *mut JitRuntime<'_>) {
    // Structural counter: the gated in-block Y-window re-publish.
    #[cfg(test)]
    super::counters::record_refresh_y();
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call (the driver is
    // blocked in the block call that reached us).
    let runtime = unsafe { &mut *rt };
    runtime.refresh_y_base();
}

/// The **local tail-call** helper: pop the current frame with the interpreter's
/// exact `call_last` semantics and hand the already-resolved successor back to the
/// block.
///
/// A linked tail call ([`super::emit::link`]) does not round-trip through the
/// driver, so it cannot let the interpreter run the instruction. The one piece of
/// `call_last` a block cannot do itself is the frame pop: the call-frame stack and
/// the Erlang stack are Rust-side `Machine` state with no fixed layout a block could
/// address. The label is *not* an issue — a module-local label is a constant, so the
/// block resolves it at **compile** time and passes the resulting word in.
///
/// This is [`Machine::pop_frame_checked`](crate::Machine) — literally the call the
/// interpreter's `Instr::CallLast` arm makes — so the frame discipline is identical
/// by construction:
///
/// * the target is resolved *before* anything is popped (at compile time; a label the
///   module does not define is not linked at all, so the block deopts to
///   [`jit_step_one`] for the interpreter's exact `unknown_label`);
/// * the pop is the interpreter's own **checked** pop, so a `Deallocate` operand that
///   no longer matches the live frame (a `trim`ed frame, or a tail call reaching past
///   the bottom frame) is the same typed error it produces, reported from the live
///   frame.
///
/// On success the resolved `target` word is returned so the block can compare it with
/// the constant it compiled in and jump straight to the successor's entry; on failure
/// the error is stored in `rt.error` and [`ERROR`] is returned, exactly as
/// [`jit_step_one`] reports a raise.
pub(crate) extern "C" fn jit_tail_call(rt: *mut JitRuntime<'_>, target: i64, deallocate: u32) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer
    // it holds points at an object the driver owns and keeps alive for the whole
    // loop (`machine`, `module`), or at a local (`labels`) that outlives the loop.
    let runtime = unsafe { &mut *rt };
    // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`;
    // no other `&mut Machine` exists while the helper runs (the driver is blocked in
    // the block call that reached us).
    let machine = unsafe { &mut *runtime.machine };

    match machine.pop_frame_checked(deallocate) {
        Ok(()) => target,
        Err(err) => {
            runtime.error = Some(err);
            ERROR
        }
    }
}

// ---------------------------------------------------------------------------
// The frame-discipline helpers: `deallocate` / `return` / `deallocate_return`
// ---------------------------------------------------------------------------
//
// The three instructions that end a function body are the last per-level control
// flow a `sum`-shaped non-tail recursion still pays a full `jit_step_one`
// interpreter step for. None of them needs the interpreter: what a block cannot do
// itself is address the call-frame stack and the Erlang stack (Rust-side `Machine`
// state with no layout a block can compute), and that is exactly what these helpers
// do — the interpreter's own calls, in the interpreter's own order, with the
// interpreter's own frame-discipline primitive ([`Machine::pop_frame_checked`]).
//
// They are byte-for-byte the interpreter's arms:
//
// * `Instr::Deallocate(n)` — [`Machine::pop_frame_checked`], then `Step::Next`;
// * `Instr::Return` — the *current* frame's continuation, i.e. the
//   `deallocate`-less half of `Instr::DeallocateReturn`'s arm;
// * `Instr::DeallocateReturn(n)` — the two in order.
//
// The continuation read is `Machine::continuation()`
// (`src/machine/frames.rs`), mapped onto the block ABI exactly as the interpreter's
// `return_step` maps it onto a [`Step`]: `Continuation::Halt` → [`HALT`] (the driver
// yields `x(0)`, C's `$RETURN` on the bottom frame), an
// `Continuation::Address { func, pc, context }` in the **current** module context →
// that `(func, pc)` as the next block target, and one in another context → the
// [`ENTER`] word plus the `(context, func, pc)` triple in [`JitRuntime::enter`] (a
// cross-module `return`, which only the driver can perform). The A.8 trace event
// `return_step` emits is deliberately not reproduced: a traced run refuses native
// entry entirely ([`super::drive`]), so `Machine::trace_on` is `false` in every
// block and the event could never be observed as missing.
//
// Every failure these helpers can produce is an `error::unsupported` **engine
// diagnostic** (a `deallocate` operand that does not match the live frame, a
// `return` with no live frame) — and the interpreter reaches those through
// `Machine::step_instruction`, whose uncaught-raise funnel
// ([`Machine::with_uncaught_frames`]) deliberately leaves a diagnostic frame-less. So
// the helpers apply the same funnel for the run's **entry** context (`0`, where the
// active module *is* the entry module) and leave a callee context for the driver,
// exactly as `step_instruction` does; for today's diagnostics that is a no-op, and it
// keeps the two paths identical if an error class ever widens.

/// Record `err` in the runtime's error slot and answer [`ERROR`] — the one failure
/// path of every in-block helper that can fail (the frame-discipline trio and the
/// heap/frame-admission family: [`jit_allocate`]/[`jit_allocate_heap`]/
/// [`jit_test_heap`]/[`jit_trim`]).
///
/// The uncaught-raise funnel is applied for the run's **entry** context only, exactly
/// as [`Machine::step_instruction`] applies it: in context `0` the active module *is*
/// the run's entry module, so `func`/`pc` and that module name every live frame, while
/// a raise inside a cross-module callee is left bare for the driver, which knows the
/// true entry module.
fn report_frame_error(
    runtime: &mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    err: BeamError,
) -> i64 {
    // SAFETY: the caller passes the driver's live, exclusively borrowed `JitRuntime`
    // reborrowed; `runtime.machine` is a non-null pointer to the driver's live
    // `Machine`, and no other `&mut Machine` exists while the helper runs (the driver
    // is blocked in the block call that reached us).
    let machine = unsafe { &mut *runtime.machine };
    let err = if machine.current_context() == 0 {
        // SAFETY: `runtime.module` points at the `&Module` the driver is running,
        // which outlives the loop; in context `0` that module is the run's entry
        // module (see `step_instruction`).
        let module = unsafe { &*runtime.module };
        machine.with_uncaught_frames(err, Some(module), func, pc)
    } else {
        err
    };
    runtime.error = Some(err);
    ERROR
}

/// Map a frame's continuation onto the block-return word, exactly as the
/// interpreter's `return_step` maps it onto a [`Step`] (see the section docs).
///
/// An `Address` in the **current** module context is the successor's `(func, pc)`
/// word — the driver looks it up in the active artifact's block table and enters it
/// (including a **continuation sub-block** entry). An `Address` in **another**
/// context is the [`ENTER`] word with the triple in [`JitRuntime::enter`]: the
/// returning function's callers may live in another module (a cross-module
/// `call_ext`), and only the driver owns the module-context registry.
///
/// # The in-block continuation
///
/// When the continuation names a block entry **of this module's compiled artifact** the
/// helper resolves the entry itself and puts it in [`JitRuntime::resume_entry`]; the
/// emitted arm then tail-jumps to it, skipping the driver iteration it would otherwise
/// cost (decode the word, `set_current_code`, the reduction bump, `ctx.reduce(1)`, the
/// entry lookup, the block prologue). `resume_entry` stays null on every other outcome, so
/// the arm keeps today's return-to-driver path for a `Halt`, a cross-context continuation,
/// an artifact that holds no code for the address and every declined bound.
///
/// There are two servable shapes, both resolved by [`resume_in_block`]:
///
/// * the **same function** the driver last entered — the shape **every** non-tail call
///   returns in when the callee is that same function, since the stored continuation is the
///   call site's own `pc + 1`, lowered as a continuation sub-block;
/// * **another function of the same artifact** — the shape every call into a *different*
///   function of the module returns in (`char_to_lower/1`'s callers). Same module, so
///   `rt.module`/`rt.labels`/`rt.literals` are already the ones the target needs and the
///   transfer is a legal `jmp` into another RX mapping; only the block-entry tables the
///   helper resolves through differ, and they are re-published for the target before the
///   jump so a nested return in the new function takes the same fast path.
fn return_word(runtime: &mut JitRuntime<'_>, continuation: Continuation) -> i64 {
    // Cleared on **every** call, so the emitted arm can never observe a stale entry from an
    // earlier return (the two arms read exactly the value their own helper wrote).
    runtime.resume_entry = std::ptr::null();
    match continuation {
        Continuation::Halt => HALT,
        Continuation::Address {
            func,
            pc,
            context,
        } => {
            // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
            // `Machine`; no other `&mut Machine` exists while the helper runs.
            let current = unsafe { (*runtime.machine).current_context() };
            if context != current {
                runtime.enter = Some((context, func, pc));
                return ENTER;
            }
            // The in-block transfer itself is shared with a frame-preserving `call`'s callee
            // (see [`enter_in_block`]): both are "continue the driver's loop at `(func, pc)`
            // without going back to the driver", so they must never diverge.
            enter_in_block(runtime, func, pc)
        }
    }
}

/// Resolve `(func, pc)` to a native block entry of the artifact the driver published and
/// perform the driver iteration the in-block transfer replaces — the **shared** helper body
/// for the tier's two in-block transfers: a `return`'s continuation ([`return_word`]) and a
/// frame-preserving `call`'s callee ([`super::call::jit_call_store_linked`]).
///
/// Clears [`JitRuntime::resume_entry`] on **every** call (so no emitted arm can observe a
/// stale entry from an earlier transfer), then:
///
/// * a **resolved** entry is stored in [`JitRuntime::resume_entry`] and the target's
///   `(func, pc)` word is returned — the arm tail-jumps to the entry;
/// * a **decline** ([`resume_in_block`]'s `Ok(None)`: no published artifact, an address no
///   block covers, a missing function, a failed lazy compile, an unsatisfied register-file
///   bound) returns the same word with `resume_entry` still null — the arm leaves the word
///   for the driver, which decodes it exactly as it did before the transfer existed;
/// * a **refused pre-emption charge** stores the host's error in [`JitRuntime::error`] and
///   returns [`ERROR`] — with **no** uncaught-raise funnel, exactly as the driver loop
///   head's own `ctx.reduce(1)?` applies none on that path.
///
/// The position the transfer publishes is the target's entry — the address a driver
/// iteration would have published — so a `ctx.reduce` refusal parks the machine exactly
/// where a driver-side refusal parks it and the resumed slice executes each instruction
/// exactly once.
pub(crate) fn enter_in_block(runtime: &mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    runtime.resume_entry = std::ptr::null();
    let word = encode_target(func, pc);
    match resume_in_block(runtime, func, pc) {
        Ok(Some(entry)) => {
            runtime.resume_entry = entry;
            word
        }
        Ok(None) => word,
        Err(err) => {
            runtime.error = Some(err);
            ERROR
        }
    }
}

/// Resolve a return's continuation to a **block entry of this artifact** and perform the
/// driver work the in-block transfer replaces — or answer `None` and leave everything to
/// the driver.
///
/// This is the one place the tier skips a driver round-trip, so it reproduces the skipped
/// iteration's work exactly, in the driver loop head's own order:
///
/// 1. the target entry is resolved — from the published tables when they already name the
///    target function ([`EntryTables`]), else from the artifact
///    ([`cross_function_entry`]). The answer declines unless the address names a block
///    (real block start first, then continuation sub-block) of **this artifact's** code —
///    the resolution is [`CompiledModule::native_entry_tables`], the same precedence
///    [`CompiledModule::entry`] gives the driver — and, for a function other than the one
///    the tables describe, unless the machine's register file satisfies that function's
///    bound;
/// 2. `Machine::set_current_code(func, pc)` — the address the driver would have published,
///    stored **before** the charge, so a refusal parks the machine exactly where a
///    driver-side refusal would have (the block about to run is untouched);
/// 3. `Machine::reductions` bumped by one, the engine-side slice counter the driver bumps
///    in the same step as the hook;
/// 4. `ExecutionContext::reduce(1)` when the run latched [`Machine::reduce_on`] — the
///    per-block-entry pre-emption charge. A refusal is returned **unchanged** and the
///    transfer is not taken;
/// 5. for a **cross-function** transfer, the target function's block-entry tables replace
///    the published ones, so a nested return inside the new function resolves through them
///    instead of falling back to the driver.
///
/// The register-file bound is not re-checked for the **same-function** shape, and that is
/// sound by construction: the bound is a property of the **function**, checked when the
/// driver entered it, and this transfer stays inside it — for a lazy artifact that is the
/// entered function's own highest natively-touched register, which covers every one of its
/// blocks, and for an eager artifact it is the whole-module maximum. `machine.x` never
/// shrinks during a run, so a bound that held then still holds now. A **different**
/// function's bound is a different number, so [`cross_function_entry`] checks it.
///
/// The block has already spilled its cached x-registers into `machine.x` before calling the
/// helper ([`super::emit::emit_helper_call`]'s pre-call spill), so a `reduce` that inspects
/// the machine sees the interpreter's exact root set — the same state the driver's own
/// charge runs in.
///
/// [`CompiledModule::native_entry_tables`]: super::code::CompiledModule::native_entry_tables
/// [`CompiledModule::entry`]: super::code::CompiledModule::entry
fn resume_in_block(
    runtime: &mut JitRuntime<'_>,
    func: u32,
    pc: u32,
) -> std::result::Result<Option<*const u8>, BeamError> {
    let Some((entry, tables)) = resolve_resume_entry(runtime, func, pc) else {
        return Ok(None);
    };
    // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`; no
    // other `&mut Machine` exists while the helper runs (the driver is blocked in the block
    // call that reached us).
    let machine = unsafe { &mut *runtime.machine };
    // The driver loop head's order: publish the position, bump the slice counter, then
    // offer the hook. Nothing of the target block has run, so a refusal is a clean
    // pre-emption rather than a replayed prefix.
    machine.set_current_code(func, pc);
    machine.reductions = machine.reductions.saturating_add(1);
    if machine.reduce_on {
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as a
        // raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };
        ctx.reduce(1)?;
    }
    if let Some(tables) = tables {
        // The transfer is taken: the function whose blocks are about to run is the one the
        // tables describe from here on.
        runtime.tables = tables;
    }
    Ok(Some(entry))
}

/// Resolve `(func, pc)` to a native entry plus, when the target is a function the published
/// tables do not describe, **that function's** tables.
///
/// The published tables answer the same-function shape with two `Vec` indexings and no
/// artifact lookup; everything else goes through the artifact, which is what makes a
/// **cross-function** continuation servable at all (a call into another function of the
/// module returns into the caller's function, whose blocks may be in another executable
/// mapping of the same artifact).
///
/// Declines — `None` — on every shape today's return-to-driver path must keep: no artifact
/// published (an interpreter step's iteration), an address the artifact compiled no block
/// for, an address in a function this module does not have, and a function whose
/// register-file bound the machine does not satisfy.
fn cross_function_entry(
    runtime: &JitRuntime<'_>,
    func: u32,
    pc: u32,
) -> Option<(*const u8, Option<EntryTables>)> {
    let compiled = runtime.compiled;
    if compiled.is_null() {
        return None;
    }
    // Structural counter: one per cross-function transfer that has to ask the artifact.
    #[cfg(test)]
    super::counters::record_cross_function_resolution();
    // SAFETY: `runtime.compiled` is the artifact the driver published for the module it is
    // running and holds alive for the whole loop (the caller-owned one in context `0`, a
    // callee's `Rc` in `DriverContext::callees`); no `&mut` to it exists while a block of it
    // runs — the compile-on-first-entry path below takes the artifact's own `RefCell` only
    // for the duration of its lookup, exactly as the driver's entry path does.
    let artifact = unsafe { &*compiled };
    // SAFETY: `runtime.module` points at the active module, which the driver holds alive for
    // the whole loop and which is the module `artifact` was compiled for (a lazy artifact
    // compiles each function from it on first entry).
    let module = unsafe { &*runtime.module };
    let (entry, bound, tables) = artifact.native_entry_tables(module, func, pc)?;
    if let Some(highest) = bound {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`; no
        // other `&mut Machine` exists while the helper runs.
        let x_len = unsafe { (*runtime.machine).x.len() };
        if x_len <= highest as usize {
            // The driver would refuse entry on its own bound check, so the machine must go
            // back to it rather than transfer into a block whose registers do not exist.
            return None;
        }
    }
    // The artifact's x write extent may have just grown: `native_entry_tables` compiles the
    // target function on first sight, and the extent the driver declared at this block's entry
    // predates that. Re-declare it before the jump (see `note_native_write_extent`).
    note_native_write_extent(runtime, artifact);
    Some((entry, Some(tables)))
}

/// Re-declare the artifact's x-register write extent on the machine.
///
/// A compiled block stores straight into `machine.x` through the pointer it holds, which the
/// `set_x` high-water mark cannot observe; the artifact bounds every such write
/// (`CompiledModule::native_write_extent`, one past the highest x slot any compiled block of it
/// touches), and [`super::enter_compiled_cached`] declares that bound before every block entry
/// so the next `Machine::reset` refills exactly what the run could have written and every
/// mid-block helper sees the full x-register prefix through `Machine::with_roots`.
///
/// **In-block transfers need it too.** [`cross_function_entry`] compiles the target function on
/// first sight, so the artifact's running maximum can grow *while a block runs*, after the
/// driver's entry-time note: the block then jumps into that function's code, which may write a
/// deeper register and call a helper that reads the register files. Declaring the (now larger)
/// extent before the jump keeps the note an upper bound. Monotone and O(1) — a no-op whenever
/// the maximum did not move; the FR file is untouched (nothing generated writes it; see
/// `CompiledModule::native_write_extent`).
fn note_native_write_extent(runtime: &JitRuntime<'_>, artifact: &CompiledModule) {
    let Some(extent) = artifact.native_write_extent() else {
        return;
    };
    // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`; no other
    // `&mut Machine` exists while a block or its helper runs (see the `cross_function_entry`
    // bound check above, which reads `x.len()` through the same pointer).
    let machine = unsafe { &mut *runtime.machine };
    machine.note_x_dirty(extent);
}

/// [`cross_function_entry`] behind the published-tables fast path: the same-function shape
/// answers without touching the artifact, and no tables need re-publishing (they already
/// describe that function).
fn resolve_resume_entry(
    runtime: &JitRuntime<'_>,
    func: u32,
    pc: u32,
) -> Option<(*const u8, Option<EntryTables>)> {
    match runtime.tables.entry(func, pc) {
        Some(entry) => Some((entry, None)),
        None => cross_function_entry(runtime, func, pc),
    }
}
/// The **`deallocate`** helper: pop the current frame with the interpreter's exact
/// `Instr::Deallocate` semantics and hand the block the address to continue at.
///
/// `pop N` is [`Machine::pop_frame_checked`] — literally the call the interpreter's
/// `Instr::Deallocate` arm makes — so a `Deallocate` operand that no longer matches
/// the live frame reports the same typed error it produces. The whole instruction is
/// one helper round-trip instead of a `jit_step_one` interpreter step.
///
/// This is the *non-terminating* member of the family: the interpreter's arm returns
/// `Step::Next`, so the block continues in-block at `pc + 1` on success and leaves
/// with [`ERROR`] otherwise. `pop_frame_checked` touches no x-register (it truncates
/// `machine.stack` and drops a frame), so the block does **not** refill its cached
/// registers after the call; it does re-publish the Y window on the continue arm when
/// a later instruction of the block still accesses one — the pop *is* a frame change.
pub(crate) extern "C" fn jit_deallocate(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    deallocate: u32,
) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it
    // holds points at an object the driver owns and keeps alive for the whole loop.
    let runtime = unsafe { &mut *rt };
    let popped = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // The same position store `Machine::step_instruction` makes at entry.
        machine.set_current_code(func, pc);
        machine.pop_frame_checked(deallocate)
    };
    match popped {
        // The pop is a frame change: `machine.stack` was truncated under the window the
        // block published, so re-publish it here — the block may continue in-block with a
        // later instruction that addresses a `Y` slot, and the emitted continue arm no
        // longer pays a separate `jit_refresh_y` call for it.
        Ok(()) => {
            runtime.refresh_y_base();
            encode_target(func, pc + 1)
        }
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}

/// The **`return`** helper: follow the current frame's continuation — the bare
/// `Instr::Return`.
///
/// `Instr::Return` pops nothing (`Machine::return_step` only reads the frame's
/// continuation), which is what distinguishes it from [`jit_deallocate_return`]; the
/// two share [`return_word`], so the mapping onto the block ABI is the one definition.
///
/// `return` **always** transfers control (the interpreter's `return_step` returns
/// `Done`/`Goto`/`Enter`, never `Next`), so the caller leaves the block with the
/// helper's word in `rax` instead of continuing at `pc + 1` — **unless** the continuation
/// names a block entry of the function already running, in which case
/// [`JitRuntime::resume_entry`] carries that entry and the emitted arm tail-jumps to it
/// (see [`return_word`]).
pub(crate) extern "C" fn jit_return(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it
    // holds points at an object the driver owns and keeps alive for the whole loop.
    let runtime = unsafe { &mut *rt };
    let continuation = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // The same position store `Machine::step_instruction` makes at entry.
        machine.set_current_code(func, pc);
        machine.take_continuation()
    };
    match continuation {
        Ok(continuation) => return_word(runtime, continuation),
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}

/// The **`deallocate_return`** helper: pop the current frame with the interpreter's
/// exact `Instr::DeallocateReturn` semantics, then follow the (now current) caller
/// frame's continuation — C's `deallocate_return`, one instruction in the loader's
/// IR and one helper round-trip here.
///
/// This is the shape **every** real compiler-produced function body ends with (the
/// loader fuses the adjacent `deallocate N; return` pair into
/// `Instr::DeallocateReturn(N)`), so a `call`-heavy module pays it once per function
/// exit on each tier. The two halves are the interpreter's, in the interpreter's
/// order — [`Machine::pop_frame_checked`] first, so a mismatched operand raises from
/// the live frame exactly as `Instr::Deallocate` does, then [`return_word`] — and the
/// position the helper records is the fused instruction's own `(func, pc)`, which is
/// what `step_instruction` records for `Instr::DeallocateReturn` (the emitter passes
/// the `return`'s `pc` when it fuses a separate pair, which is the last instruction
/// the interpreter would have recorded in that case).
///
/// Like [`jit_return`] it always transfers control, so the caller leaves the block
/// with the helper's word in `rax` — or tail-jumps to the same-function block entry
/// [`return_word`] resolved.
pub(crate) extern "C" fn jit_deallocate_return(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    deallocate: u32,
) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it
    // holds points at an object the driver owns and keeps alive for the whole loop.
    let runtime = unsafe { &mut *rt };
    let outcome = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // The same position store `Machine::step_instruction` makes at entry.
        machine.set_current_code(func, pc);
        machine.pop_frame_checked(deallocate).and_then(|()| machine.take_continuation())
    };
    match outcome {
        Ok(continuation) => return_word(runtime, continuation),
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}

// ---------------------------------------------------------------------------
// The heap / frame-admission family
// ---------------------------------------------------------------------------

/// The **`allocate`** helper: push a stack frame with the interpreter's exact
/// `Instr::Allocate` semantics.
///
/// The interpreter's arm is `Machine::push_frame(stack_need)` and nothing else — it
/// deliberately **skips** C's heap check (C's `allocate Ns Live` is `$AH(Ns,0,Live)`,
/// but this engine's `Instr::Allocate` only pushes the frame; the admission point is
/// `Instr::AllocateHeap`/`Instr::TestHeap`), so this helper makes exactly that call and
/// never admits a heap need.
///
/// This is the *non-terminating* member of the family: the interpreter's arm returns
/// `Step::Next`, so the block continues in-block at `pc + 1` on success (and leaves with
/// [`ERROR`] otherwise — `push_frame` cannot fail today, but the funnel is shared).
/// `push_frame` touches no x-register, so the block refills nothing — but it **is** a
/// frame change, so the Y window is re-published on the continue arm when a later
/// instruction of the block still accesses a Y slot.
pub(crate) extern "C" fn jit_allocate(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    stack_need: u32,
) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it
    // holds points at an object the driver owns and keeps alive for the whole loop.
    let runtime = unsafe { &mut *rt };
    let pushed = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // The same position store `Machine::step_instruction` makes at entry.
        machine.set_current_code(func, pc);
        machine.push_frame_window(stack_need)
    };
    match pushed {
        // The push is a frame change (`machine.stack` may have been grown, and the window
        // now belongs to the *new* frame), so publish it here: the block continues
        // in-block after an `allocate`, and the emitted continue arm no longer pays a
        // separate `jit_refresh_y` call for it. The publish uses the `(y_base, y_count)`
        // the push just computed rather than re-deriving them.
        Ok((y_base, y_count)) => {
            runtime.publish_y_window(y_base, y_count);
            encode_target(func, pc + 1)
        }
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}

/// The **`allocate_heap`** helper: admit the heap need, **then** push the frame — the
/// interpreter's `Instr::AllocateHeap` order (`ensure_heap` first, `push_frame` after).
///
/// It is the one instruction of the family that reaches the host's heap-admission door
/// ([`ExecutionContext::ensure_heap_full`] through [`Machine::ensure_heap`]), passing the
/// interpreter's own `heap_need`/`live` operands, so a collecting host sees **exactly
/// one** admission point per execution with the interpreter's full root set (the block
/// has spilled its cached x-registers first, so `machine.x` is authoritative). Because a
/// collecting host rewrites `machine.x` in place, the block **must** reload its cached
/// registers on the continue arm (see [`super::emit::heap`]).
///
/// The non-terminating shape: the interpreter returns `Step::Next`, so the block
/// continues in-block at `pc + 1`; anything else (a heap-admission failure) leaves with
/// [`ERROR`].
pub(crate) extern "C" fn jit_allocate_heap(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    stack_need: u32,
    heap_need: u32,
    live: u32,
) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it
    // holds points at an object the driver owns and keeps alive for the whole loop.
    let runtime = unsafe { &mut *rt };
    let pushed = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed
        // as a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };
        // The same position store `Machine::step_instruction` makes at entry.
        machine.set_current_code(func, pc);
        // The interpreter's own order: admit (`ensure_heap`), then push the frame.
        machine
            .ensure_heap(heap_need, live, ctx)
            .and_then(|()| machine.push_frame_window(stack_need))
    };
    match pushed {
        // The push is a frame change and the admission before it may have collected, so
        // publish the window here; the emitted continue arm keeps its register refill
        // (a collecting host rewrote `machine.x`) but no longer pays a separate
        // `jit_refresh_y` call.
        Ok((y_base, y_count)) => {
            runtime.publish_y_window(y_base, y_count);
            encode_target(func, pc + 1)
        }
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}

/// The **`test_heap`** helper: admit the heap need with the interpreter's exact
/// `Instr::TestHeap` semantics.
///
/// Like [`jit_allocate_heap`]'s first half it reaches the host's heap-admission door
/// exactly once per execution with the interpreter's own `heap_need`/`live` operands,
/// and the collecting host may relocate the cached x-registers — so the block reloads
/// them on the continue arm. The instruction is otherwise the non-terminating shape.
pub(crate) extern "C" fn jit_test_heap(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    heap_need: u32,
    live: u32,
) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it
    // holds points at an object the driver owns and keeps alive for the whole loop.
    let runtime = unsafe { &mut *rt };
    let admitted = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed
        // as a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };
        // The same position store `Machine::step_instruction` makes at entry.
        machine.set_current_code(func, pc);
        machine.ensure_heap(heap_need, live, ctx)
    };
    match admitted {
        Ok(()) => encode_target(func, pc + 1),
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}

/// The **`trim`** helper: shrink the current frame with the interpreter's exact
/// `Instr::Trim` semantics ([`Machine::trim`], the interpreter's own call), so an
/// over-trim raises the same typed error the interpreter's arm does.
///
/// The non-terminating shape: the interpreter returns `Step::Next`, so the block
/// continues in-block at `pc + 1`. `trim` touches no x-register (it shifts
/// `machine.stack` and renumbers the frame's Y slots), so the block refills nothing —
/// but it changes the frame, so the Y window is re-published on the continue arm when a
/// later instruction of the block still accesses a Y slot.
pub(crate) extern "C" fn jit_trim(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    n: u32,
) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it
    // holds points at an object the driver owns and keeps alive for the whole loop.
    let runtime = unsafe { &mut *rt };
    let trimmed = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs.
        let machine = unsafe { &mut *runtime.machine };
        // The same position store `Machine::step_instruction` makes at entry.
        machine.set_current_code(func, pc);
        machine.trim(n)
    };
    match trimmed {
        // `trim` moves the frame's base, so re-publish the window here; the block continues
        // in-block after it and the emitted continue arm no longer pays a separate
        // `jit_refresh_y` call.
        Ok(()) => {
            runtime.refresh_y_base();
            encode_target(func, pc + 1)
        }
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}
