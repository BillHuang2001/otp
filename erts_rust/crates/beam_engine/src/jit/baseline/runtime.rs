//! The baseline JIT's block ABI and its per-instruction helper.
//!
//! The `HALT`/`ERROR`/`WAIT` return words and the `(func, pc)` control-word codec,
//! the `#[repr(C)]` runtime block a compiled block receives, and the two helpers a
//! block calls: [`jit_step_one`] — the per-instruction deopt for the first
//! instruction the block could not lower natively (a suspending `receive` is handed
//! back to the driver through [`WAIT_BIT`], so a blocking-aware driver can report it
//! as a suspension) — and [`jit_tail_call`], the checked frame pop a **linked local
//! tail call** uses (see [`super::emit::link`]). See `super` for the block ABI as a
//! whole, the emitter that produces blocks and the driver that decodes the words
//! these items produce.
#![allow(unsafe_code)]

use crate::interpreter::LabelMap;
use std::mem;

use beam_common::BeamError;
use beam_loader::Module;
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::machine::Machine;
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
/// or `None` for every other word (a real continuation, [`HALT`] or [`ERROR`]).
pub const fn decode_wait(word: i64) -> Option<(u32, u32)> {
    if word >= 0 || word == HALT || word == ERROR {
        return None;
    }
    decode_target(word & !WAIT_BIT)
}

/// The one argument a compiled block receives.
///
/// A `#[repr(C)]` bag of raw pointers at the live driver-owned objects (valid for
/// the whole driver call, never aliased while a `&mut` derived from a field is
/// alive) plus the slot a helper stores a propagating [`BeamError`] in. Generated
/// code touches **only offset 0 and 8** ([`RUNTIME_MACHINE_DISP`],
/// [`RUNTIME_XREGS_DISP`]); the remaining fields are read by the Rust-side
/// [`jit_step_one`] helper.
///
/// `machine` is the [`Machine`] itself (what the helper needs); `x_regs` is the base
/// of its **x-register array** (`machine.x.as_ptr()`), which is what a block indexes.
/// The two are separate fields because a block's `[base + n*8]` addressing needs the
/// array's data pointer, not the address of the `Vec` field inside a non-`repr(C)`
/// Rust struct. `machine.x` is only resized by [`Machine::reset`] (which runs before
/// this pointer is taken) — `ensure_heap` hands the context a slice, so it cannot
/// reallocate the array mid-run.
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
}

/// A compiled block's native signature.
pub(crate) type NativeBlock = for<'a> extern "C" fn(*mut JitRuntime<'a>) -> i64;

/// The `jit_step_one` helper's native signature (the block ABI plus the
/// instruction address).
pub(crate) type StepHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32) -> i64;

/// The `jit_tail_call` helper's native signature (the block ABI plus the resolved
/// successor word and the frame size a local `call_last` pops).
pub(crate) type TailHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, i64, u32) -> i64;

/// The byte offset of [`JitRuntime::machine`] — the field the helper scaffolding is
/// handed; generated code reads [`JitRuntime::x_regs`] instead.
pub(crate) const RUNTIME_MACHINE_DISP: i32 = 0;

/// The byte offset of [`JitRuntime::x_regs`] (the base of `machine.x`) — the **only**
/// field generated code loads indirectly. It is the second `#[repr(C)]` pointer
/// field, hence one pointer width past `machine`.
pub(crate) const RUNTIME_XREGS_DISP: i32 =
    RUNTIME_MACHINE_DISP + mem::size_of::<*const u8>() as i32;

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

    match machine.step_instruction(module, labels, func, pc, ctx) {
        Ok(Step::Next) => encode_target(func, pc + 1),
        Ok(Step::Goto { func, pc }) => encode_target(func, pc),
        // Hand the resume address to the driver (see `WAIT_BIT`): a blocking-aware
        // driver reports it as `RunOutcome::Wait`, a non-blocking one folds it into
        // the typed `blocked` error (never a wrong result and never a silent
        // continue at the `wait`'s own re-examination label).
        Ok(Step::Wait { func, pc }) => encode_wait(func, pc),
        // A module-context switch (a `call_ext*` the host resolved through
        // `ExecutionContext::resolve_external_call`) is an **interpreter-driver**
        // capability: a compiled block's module, label map and literal area are all
        // bound for one module, so a tier cannot execute another module's code.
        // Reported as a typed error, never a silently wrong block.
        Ok(Step::Enter { .. }) => {
            runtime.error = Some(crate::error::unsupported(
                "jit: module context switch outside the interpreter driver",
            ));
            ERROR
        }
        Ok(Step::Done) => HALT,
        Err(err) => {
            runtime.error = Some(err);
            ERROR
        }
    }
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
