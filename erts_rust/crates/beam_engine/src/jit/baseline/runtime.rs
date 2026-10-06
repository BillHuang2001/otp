//! The baseline JIT's block ABI and its per-instruction helper.
//!
//! The `HALT`/`ERROR` sentinels and the `(func, pc)` control-word codec, the
//! `#[repr(C)]` runtime block a compiled block receives, and [`jit_step_one`] —
//! the helper a block calls for the first instruction it could not lower
//! natively. See `super` for the block ABI as a whole, the emitter that produces
//! blocks and the driver that decodes the words these items produce.
#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::mem;

use beam_common::BeamError;
use beam_loader::{Label, Module};
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
    pub(crate) labels: *const BTreeMap<Label, (u32, u32)>,
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
/// * [`Step::Wait { .. }`] → the typed [`crate::error::blocked`] error is stored in
///   `rt.error` and [`ERROR`] is returned (the native tiers are **non-blocking**
///   entries: a `receive` that finds no message cannot suspend them),
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
        // The native tiers are non-blocking entries: a `receive` with no message
        // available cannot suspend them, so the suspension surfaces as the same
        // typed error the interpreter-only `run_export` reports (never a wrong
        // result and never a silent continue).
        Ok(Step::Wait { .. }) => {
            runtime.error = Some(crate::error::blocked());
            ERROR
        }
        Ok(Step::Done) => HALT,
        Err(err) => {
            runtime.error = Some(err);
            ERROR
        }
    }
}
