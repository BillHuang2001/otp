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
/// or `None` for every other word (a real continuation, [`HALT`], [`ERROR`] or
/// [`ENTER`]).
pub const fn decode_wait(word: i64) -> Option<(u32, u32)> {
    if word >= 0 || word == HALT || word == ERROR || word == ENTER {
        return None;
    }
    decode_target(word & !WAIT_BIT)
}

/// The one argument a compiled block receives.
///
/// A `#[repr(C)]` bag of raw pointers at the live driver-owned objects (valid for
/// the whole driver call, never aliased while a `&mut` derived from a field is
/// alive) plus the slot a helper stores a propagating [`BeamError`] in. Generated
/// code touches **only offsets 0, 8, 16 and 24** ([`RUNTIME_MACHINE_DISP`],
/// [`RUNTIME_XREGS_DISP`], [`RUNTIME_YBASE_DISP`], [`RUNTIME_YEND_DISP`]); the
/// remaining fields are read by the Rust-side [`jit_step_one`] helper.
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
    /// Read and written **only by Rust** (the driver and [`jit_step_one`]); the
    /// generated code loads no field past [`RUNTIME_YEND_DISP`].
    pub(crate) enter: Option<(u32, u32, u32)>,
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
}
/// A compiled block's native signature.
pub(crate) type NativeBlock = for<'a> extern "C" fn(*mut JitRuntime<'a>) -> i64;

/// The `jit_step_one` helper's native signature (the block ABI plus the
/// instruction address).
pub(crate) type StepHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32) -> i64;

/// The `jit_tail_call` helper's native signature (the block ABI plus the resolved
/// successor word and the frame size a local `call_last` pops).
pub(crate) type TailHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, i64, u32) -> i64;

/// The `jit_deallocate`/`jit_deallocate_return` helpers' native signature (the block ABI
/// plus the instruction's own `(func, pc)` and the frame size the checked pop must match).
pub(crate) type FrameExitHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32) -> i64;

/// The `jit_return` helper's native signature (the block ABI plus the instruction's own
/// `(func, pc)`); a bare `return` pops nothing, so it carries no frame size.
pub(crate) type ReturnHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32) -> i64;

/// The `jit_ext_call` helper's native signature (the block ABI plus the
/// instruction's own `(func, pc)`, the `ImportRef` index, the [`EXT_CALL`]-family
/// kind and — for `call_ext_last` — the frame size to pop).
pub(crate) type ExtCallHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32, u32, u32) -> i64;
/// The `jit_set_position` helper's native signature (the block ABI plus the block's
/// own `(func, pc)`); it returns nothing — a position store cannot fail.
///
/// [`jit_publish_y`] shares it: the Y-window publish is the position store plus the
/// window refresh, and is emitted through the same three-argument call sequence.
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

// ---------------------------------------------------------------------------
// The helper
// ---------------------------------------------------------------------------

/// Fold a [`Step`] (or a propagating raise) into the block-return word — the **one**
/// definition of the block-ABI encoding, shared by every helper that runs
/// interpreter semantics ([`jit_step_one`], [`jit_step_ext`]).
///
/// `func`/`pc` are the address of the instruction the helper ran; they are used only
/// for [`Step::Next`], which advances to `pc + 1`.
pub(crate) fn fold_step(
    runtime: &mut JitRuntime<'_>,
    step: std::result::Result<Step, Box<BeamError>>,
    func: u32,
    pc: u32,
) -> i64 {
    match step {
        Ok(Step::Next) => encode_target(func, pc + 1),
        Ok(Step::Goto { func, pc }) => encode_target(func, pc),
        // Hand the resume address to the driver (see `WAIT_BIT`): a blocking-aware
        // driver reports it as `RunOutcome::Wait`, a non-blocking one folds it into
        // the typed `blocked` error (never a wrong result and never a silent
        // continue at the `wait`'s own re-examination label).
        Ok(Step::Wait { func, pc }) => encode_wait(func, pc),
        // A module-context transfer (a `call_ext*` the host resolved through
        // `ExecutionContext::resolve_external_call`, a `return` onto a
        // caller-context continuation, or a raise landing in another context's catch
        // frame). A compiled block's code, label map and literal area are all bound
        // for one module, so the block cannot make the switch itself: it hands the
        // resolved target to the driver, which owns the module-context registry
        // (`Machine::switch_context`) exactly as the interpreter's driver does.
        Ok(Step::Enter {
            context,
            func,
            pc,
        }) => {
            runtime.enter = Some((context, func, pc));
            ENTER
        }
        Ok(Step::Done) => HALT,
        Err(err) => {
            runtime.error = Some(*err);
            ERROR
        }
    }
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
/// * [`Step::Done`] → [`HALT`],
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
    // SAFETY: `runtime.labels` points at the driver's `labels` local, which outlives
    // the loop.
    let labels = unsafe { &*runtime.labels };
    // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
    // a raw pointer; the driver holds no `&mut` to it while the helper runs.
    let ctx = unsafe { &mut *runtime.ctx };

    let step = machine.step_instruction(module, labels, func, pc, ctx);
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
            _ => machine.step_instruction(module, labels, func, pc, ctx),
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

/// The **position store** helper: record a compiled block's own `(func, pc)` as the
/// machine's current execution position.
///
/// Every compiled block calls this from its **prologue**, so
/// [`Machine::current_code`](crate::Machine::current_code) (and the derived
/// `process_info/2` position items) reflect block-granularity position even across
/// blocks reached by a *linked* branch — a linked jump/tail call never returns to the
/// driver, so a store only in the driver's loop would miss exactly those. A parked
/// `receive` and a raise stay exact because both go through the interpreter step
/// ([`jit_step_one`] → [`Machine::step_instruction`](crate::Machine)), which records
/// the *instruction* address itself.
///
/// This is [`Machine::set_current_code`](crate::Machine) — the accessor the setter
/// exists for — reached through the block ABI. It is infallible and touches no
/// register file, so it is safe to call with the block's cached x-registers live in
/// the callee-saved registers; it clobbers only the caller-saved ones.
pub(crate) extern "C" fn jit_set_position(rt: *mut JitRuntime<'_>, func: u32, pc: u32) {
    // Structural counter: one Y-free block prologue.
    #[cfg(test)]
    super::counters::record_set_position();
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; `runtime.machine`
    // points at the driver's live `Machine`, and no other `&mut Machine` exists while
    // the helper runs (the driver is blocked in the block call that reached us).
    let runtime = unsafe { &mut *rt };
    // SAFETY: see above — `runtime.machine` is a non-null pointer to the driver's live
    // `Machine`.
    let machine = unsafe { &mut *runtime.machine };
    machine.set_current_code(func, pc);
}

/// The **Y-window publish** helper: re-publish the current frame's Y window *and*
/// record the block's entry position, in one call.
///
/// It is [`jit_set_position`] plus [`JitRuntime::refresh_y_base`], and a block **whose
/// body accesses a Y slot** calls it from its prologue *in place of* the position store
/// — so such a block pays no extra call for publishing its window, and a block that
/// never touches a Y slot keeps emitting the plain position store (it pays nothing).
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
/// prologue loads the cached x-registers (exactly like [`jit_set_position`]).
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
/// path of the three frame-discipline helpers.
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
fn return_word(runtime: &mut JitRuntime<'_>, continuation: Continuation) -> i64 {
    match continuation {
        Continuation::Halt => HALT,
        Continuation::Address {
            func,
            pc,
            context,
        } => {
            // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
            // `Machine`; no other `&mut Machine` exists while the helper runs.
            let machine = unsafe { &mut *runtime.machine };
            if context == machine.current_context() {
                encode_target(func, pc)
            } else {
                runtime.enter = Some((context, func, pc));
                ENTER
            }
        }
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
        Ok(()) => encode_target(func, pc + 1),
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
/// helper's word in `rax` instead of continuing at `pc + 1`.
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
        machine.continuation()
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
/// with the helper's word in `rax`.
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
        machine.pop_frame_checked(deallocate).and_then(|()| machine.continuation())
    };
    match outcome {
        Ok(continuation) => return_word(runtime, continuation),
        Err(err) => report_frame_error(runtime, func, pc, err),
    }
}
