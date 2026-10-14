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

//! The baseline JIT's operand readers: the x-register index an operand names and the
//! raw `Eterm` word of a constant operand (see [`crate::operand::read_operand`], the
//! seam both must agree with) — plus the **shared guarded Y-window access** every
//! lowering that reads or writes a `Y` slot emits (see the section comment).

use std::mem;

use beam_common::Result;
use beam_loader::{Operand, Reg as RegRef};
use beam_term::{make_atom, make_small, Eterm, MAX_SMALL, MIN_SMALL, NIL};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::runtime::{RUNTIME_YBASE_DISP, RUNTIME_YEND_DISP};
use super::RUNTIME_SLOT_DISP;

/// The byte offset of x-register `x` inside `machine.x` (a `[Eterm]` of 8-byte
/// words).
pub(super) fn x_offset(x: u32) -> i32 {
    (x as i32) * (mem::size_of::<Eterm>() as i32)
}

/// The byte offset of Y-slot `k` inside the current frame's Y area (a `[Eterm]` of
/// 8-byte words), i.e. the displacement an emitted `lea [y_base + k*8]` carries.
///
/// `k` must satisfy [`y_addressable`]; beyond that bound the `disp32` would not fit,
/// so `fast_arith`/`fast_source` refuse the operand at **compile** time (a deopt is
/// always correct — the interpreter runs the instruction).
pub(super) fn y_offset(k: u32) -> i32 {
    (k as i32) * (mem::size_of::<Eterm>() as i32)
}

/// Whether Y-slot `k` is an index the emitted `lea ... disp32` can address.
///
/// A Y index an actual frame could never have (the engine's frames hold far fewer
/// slots, and `y_offset` would wrap `i32`) is refused: the fast path then deopts to
/// the interpreter, whose own `y(k)` bounds check reports the faithful error.
pub(crate) fn y_addressable(k: u32) -> bool {
    k <= (i32::MAX / mem::size_of::<Eterm>() as i32) as u32
}

/// The x-register index an operand names, if any (`X(n)` or the `tr`-annotated
/// `Typed { reg: X(n), .. }` — the `Type` annotation is irrelevant to the value).
pub(crate) fn x_register(operand: &Operand) -> Option<u32> {
    match operand {
        Operand::X(n) => Some(*n),
        Operand::Typed {
            reg: RegRef::X(n),
            ..
        } => Some(*n),
        _ => None,
    }
}

/// The Y-slot index an operand names, if any (`Y(k)` or the `tr`-annotated
/// `Typed { reg: Y(k), .. }` — the `Type` annotation is irrelevant to the value).
///
/// The caller decides addressability ([`y_addressable`]); this is the purely
/// syntactic read, mirroring [`x_register`].
pub(crate) fn y_register(operand: &Operand) -> Option<u32> {
    match operand {
        Operand::Y(n) => Some(*n),
        Operand::Typed {
            reg: RegRef::Y(n),
            ..
        } => Some(*n),
        _ => None,
    }
}
/// The raw `Eterm` word of a constant operand, as [`crate::operand::read_operand`]
/// evaluates it (`make_small`/`make_atom`/`NIL`).
///
/// An `Integer`/`Character` outside the small-int range is `None`: the native tier
/// only inlines immediates, so such a constant makes the block **deopt** to
/// `jit_step_one`, whose interpreter step materialises the exact bignum through
/// [`crate::terms::make_integer`]. `make_small` must never see an out-of-range value
/// (it `debug_assert`s).
pub(crate) fn constant_word(operand: &Operand) -> Option<i64> {
    match operand {
        Operand::Integer(value) => small_word(*value),
        Operand::Character(c) => small_word(i64::from(*c)),
        Operand::Atom(atom) => Some(make_atom(*atom).raw() as i64),
        Operand::Nil => Some(NIL.raw() as i64),
        _ => None,
    }
}

/// The raw `Eterm` word of a small-int constant, or `None` when `value` is outside
/// the 60-bit immediate range (`beam_term::{MIN_SMALL, MAX_SMALL}`).
pub(super) fn small_word(value: i64) -> Option<i64> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        Some(make_small(value).raw() as i64)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The guarded Y-window access (shared by every Y-slot lowering)
// ---------------------------------------------------------------------------
//
// A `Y` operand is a slot in the **current call frame's** Y area, which lives in
// `machine.stack` — a Rust `Vec` that a frame push may reallocate. The runtime
// therefore publishes the live frame's Y window (`JitRuntime::y_base`/`y_end`,
// refreshed by the driver before every native entry and by every frame-changing
// helper) and every emitted Y access is **guarded** against it:
//
// ```text
//   mov  addr,  [rsp + RUNTIME_SLOT_DISP]  ; rt (the block's own stack slot)
//   mov  end,   [addr + RUNTIME_YEND_DISP] ; y_end
//   mov  addr,  [addr + RUNTIME_YBASE_DISP]; y_base
//   lea  addr,  [addr + k*8]               ; the slot address
//   cmp  addr,  end
//   jae  fallback                          ; slot >= y_end -> outside the frame
//   <the access: `mov dst, [addr]` for a load, `mov [addr], src` for a store>
// ```
//
// The guard can only ever **reject**: a slot outside the frame jumps to the same
// `jit_step_one` fallback as any other un-lowered shape, and the interpreter's own
// `y(k)` bounds check then produces exactly its error — so no out-of-frame
// dereference and no wrong value is possible. The frame-less case is covered too:
// `y_base == y_end == null` makes `lea k*8; cmp; jae` reject every slot. A Y index so
// large that [`y_offset`] would overflow the `disp32` is refused at compile time
// ([`y_addressable`]).
//
// The helpers are the **single** home of the guard: [`emit_guarded_y_load`] serves
// the arithmetic fast path's Y operand (`arith::load_into`) and a Y-source `move`,
// [`emit_guarded_y_store`] serves its Y destination (`arith::try_inline_gc_bif`) and a
// Y-destination `move`. Reloading `rt` out of the block's own stack slot
// (`[rsp + RUNTIME_SLOT_DISP]`) is the established idiom (`super::emit_helper_call`
// does the same for every helper call).

/// Emit the guarded address of Y slot `k` into `addr`, using `end` as a scratch for
/// the window bound, and jump to `fallback` when the slot lies outside the current
/// frame's Y window (or no frame is live — both window pointers null).
///
/// On success `addr` holds `y_base + k*8`. `addr` and `end` must be distinct
/// caller-saved registers that are dead at the access; neither may be a cache
/// register ([`super::CACHE_REGISTERS`]) or the block's reserved `rbx`/`rsp`.
pub(super) fn emit_guarded_y_address(
    buffer: &mut CodeBuffer,
    addr: Reg,
    end: Reg,
    k: u32,
    fallback: CodeLabel,
) -> Result<()> {
    buffer.mov_rm(addr, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_rm(end, addr, RUNTIME_YEND_DISP)?;
    buffer.mov_rm(addr, addr, RUNTIME_YBASE_DISP)?;
    buffer.lea(addr, addr, y_offset(k))?;
    buffer.cmp_rr(addr, end)?;
    buffer.jcc(Cond::Ae, fallback)
}

/// Emit a **guarded** read of Y slot `k` into `value`, jumping to `fallback` when the
/// slot lies outside the current frame's Y window (see [`emit_guarded_y_address`]).
///
/// `value` and `end` must be distinct caller-saved scratch registers; `value` receives
/// the slot's word.
pub(super) fn emit_guarded_y_load(
    buffer: &mut CodeBuffer,
    value: Reg,
    end: Reg,
    k: u32,
    fallback: CodeLabel,
) -> Result<()> {
    #[cfg(test)]
    let before = buffer.len();
    emit_guarded_y_address(buffer, value, end, k, fallback)?;
    buffer.mov_rm(value, value, 0)?;
    #[cfg(test)]
    super::super::counters::record_y_access(true, k, (buffer.len() - before) as u64);
    Ok(())
}

/// Emit a **guarded** store of `value` into Y slot `k`, jumping to `fallback` when the
/// slot lies outside the current frame's Y window (see [`emit_guarded_y_address`]).
///
/// `value`, `addr` and `end` must be three distinct caller-saved scratch registers;
/// `value` is preserved and `addr`/`end` are clobbered. Nothing is committed before the
/// store, so a rejection leaves the machine exactly as the interpreter will re-run it.
pub(super) fn emit_guarded_y_store(
    buffer: &mut CodeBuffer,
    value: Reg,
    addr: Reg,
    end: Reg,
    k: u32,
    fallback: CodeLabel,
) -> Result<()> {
    #[cfg(test)]
    let before = buffer.len();
    emit_guarded_y_address(buffer, addr, end, k, fallback)?;
    buffer.mov_mr(addr, 0, value)?;
    #[cfg(test)]
    super::super::counters::record_y_access(false, k, (buffer.len() - before) as u64);
    Ok(())
}
