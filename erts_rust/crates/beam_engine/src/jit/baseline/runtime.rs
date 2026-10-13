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

use crate::interpreter::LabelMap;
use std::mem;

use beam_common::BeamError;
use beam_loader::{ImportRef, Module};
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
/// them; the pair is refreshed by [`JitRuntime::refresh_y_base`], which the driver runs
/// before every native entry and every frame-changing helper runs at its end — the
/// Erlang stack is a Rust `Vec` that `push_frame` may reallocate, so the window must
/// never outlive the frame it names. When no frame is live both are null, which makes
/// every emitted Y access reject (see the Y-window guard in the arithmetic fast path).
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
    /// Null when no frame is live. Read by generated code at [`RUNTIME_YEND_DISP`].
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
    /// Called by the driver before every native entry and by every helper that can
    /// change the frame stack or **reallocate** `machine.stack` ([`jit_step_one`],
    /// [`jit_tail_call`]) at the end of its own work. `Machine::push_frame` grows the
    /// Erlang stack with `Vec::resize`, so a window taken before a frame push can name
    /// freed memory; refreshing here is what keeps the published window always current.
    /// No inlined shape pushes a frame or grows the stack (only a helper does), so
    /// refreshing at those two helpers covers every in-block continue as well.
    ///
    /// With no live frame both fields are null: `lea k*8; cmp; jae` then rejects every
    /// Y access (the emitted guard), so a frame-less block can never dereference one.
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

/// The `jit_ext_call` helper's native signature (the block ABI plus the
/// instruction's own `(func, pc)`, the `ImportRef` index, the [`EXT_CALL`]-family
/// kind and — for `call_ext_last` — the frame size to pop).
pub(crate) type ExtCallHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32, u32, u32, u32) -> i64;
/// The `jit_set_position` helper's native signature (the block ABI plus the block's
/// own `(func, pc)`); it returns nothing — a position store cannot fail.
pub(crate) type PositionHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32);

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

/// The per-instruction deopt: execute exactly one instruction with the
/// interpreter's own semantics.
///
/// This is the block ABI's helper. A block calls it (with `rt`, the instruction's
/// `func` and `pc`) for the first instruction it could not lower natively; the
/// helper runs [`Machine::step_instruction`](crate::Machine) — *the* interpreter
/// step — and maps its result onto the same return word the block ABI uses:
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
///
/// The helper re-publishes the current frame's Y window
/// ([`JitRuntime::refresh_y_base`]) on **every** return path: the instruction it just
/// executed may have pushed a frame, `trim`med one or **reallocated** `machine.stack`
/// (an `allocate`'s `push_frame`), and the block continues in-block at `pc + 1` on
/// `Step::Next` without returning to the driver — so the window the next Y access
/// reads must be this helper's own post-instruction state, never a stale pointer into
/// a freed `Vec` buffer.
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

    let word = match machine.step_instruction(module, labels, func, pc, ctx) {
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
    };
    // The executed instruction may have changed the frame stack (see the docs above),
    // so re-publish the Y window before the block continues in-block or exits.
    runtime.refresh_y_base();
    word
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
/// frame discipline that failed — falls back to [`jit_step_one`], which runs the
/// *entire* instruction through the interpreter. That fallback is what keeps the
/// synchronous path and every one of its errors byte-identical to the tier before
/// this arm existed: nothing is mutated before the fallback, so the interpreter sees
/// exactly the state it would have seen and reports exactly its own error.
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
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed
        // as a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        resolve_ext_entry(
            machine,
            module,
            ImportRef(target),
            kind,
            func,
            pc,
            deallocate,
            ctx,
        )
    };
    match resolved {
        Some((context, callee, callee_pc)) => {
            // SAFETY: `rt` is the same live, exclusively borrowed `JitRuntime` the
            // driver owns; the `&mut` derived from it above is dead by now, so this
            // store does not alias it. The driver takes the slot when it decodes the
            // `ENTER` word.
            unsafe { (*rt).enter = Some((context, callee, callee_pc)) };
            ENTER
        }
        // The instruction is the interpreter's to execute (all of it, exactly once).
        None => jit_step_one(rt, func, pc),
    }
}

/// Resolve a `call_ext*` through the host's seam and apply its frame discipline.
///
/// `None` means "the interpreter step must run this instruction": the host cannot run
/// the callee in this machine, the import does not resolve, the callee's entry label
/// is not in its own label index, or the frame discipline failed. **Nothing has been
/// mutated** on any of those paths (registering a module context is idempotent, the
/// continuation store and the frame pop both validate before writing), so the caller's
/// [`jit_step_one`] fallback re-runs the instruction and produces exactly what the
/// interpreter would have produced.
fn resolve_ext_entry(
    machine: &mut Machine,
    module: &Module,
    target: ImportRef,
    kind: u32,
    func: u32,
    pc: u32,
    deallocate: u32,
    ctx: &mut dyn ExecutionContext,
) -> Option<(u32, u32, u32)> {
    // The import's `(module, function, arity)`. This is the per-call resolution
    // (`LabelMap::import_target`'s uncached fallback), which is the definition of the
    // import semantics: an index past the end of `ImpT` and an atom the module's table
    // does not hold are not resolvable, so the interpreter must report them.
    let import = module.imports.get(target.0 as usize)?;
    let module_name = module.atom_name(import.module)?;
    let function_name = module.atom_name(import.function)?;
    let (callee, label) = ctx
        .resolve_external_call(module_name, function_name, import.arity)
        .ok()??;
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
    Some((context, callee_func, callee_pc))
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
/// The helper ends by re-publishing the current frame's Y window
/// ([`JitRuntime::refresh_y_base`]): the pop changed which frame is on top, and the
/// block jumps straight into the successor's code (no driver round-trip), so any Y
/// access there must see the successor frame's window.
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

    let word = match machine.pop_frame_checked(deallocate) {
        Ok(()) => target,
        Err(err) => {
            runtime.error = Some(err);
            ERROR
        }
    };
    // The pop removed the top frame, so the Y window the block may still read (a
    // `call_last` is followed by the successor's code) now belongs to the frame below.
    runtime.refresh_y_base();
    word
}
