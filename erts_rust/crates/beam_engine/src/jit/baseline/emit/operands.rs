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
//! lowering that reads or writes a `Y` slot emits (see the section comment), and the
//! [`Loc`] location abstraction (an in-bound x-register *or* an addressable `Y` slot)
//! with the guarded load/guard/store primitives over it that every widened lowering
//! (the aggregate readers/writer, `swap`, `init_yregs`) shares.

use std::mem;

use beam_common::Result;
use beam_loader::{Operand, Reg as RegRef};
use beam_term::{make_atom, make_small, Eterm, MAX_SMALL, MIN_SMALL, NIL};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::super::runtime::{RUNTIME_YBASE_DISP, RUNTIME_YEND_DISP};
use super::{touch, Allocation, RUNTIME_SLOT_DISP};

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

/// Emit only the **guard** of a `Y` destination, leaving its slot address in `addr`
/// for a later [`Loc::store_at`], and record the guarded store.
///
/// A lowering that writes **more than one** destination front-loads *every* `Y`
/// destination's guard through this before committing any store, so a rejecting guard
/// can never leave a partially written destination behind for the interpreter's re-run
/// (on the `fallback` path) to observe. See [`emit_guarded_y_store`] for the guard
/// itself; a `Loc::X` emits nothing (there is no frame window to check).
pub(super) fn guard_y_store(
    buffer: &mut CodeBuffer,
    k: u32,
    addr: Reg,
    end: Reg,
    fallback: CodeLabel,
) -> Result<()> {
    #[cfg(test)]
    let before = buffer.len();
    emit_guarded_y_address(buffer, addr, end, k, fallback)?;
    #[cfg(test)]
    super::super::counters::record_y_access(false, k, (buffer.len() - before) as u64);
    Ok(())
}

// ---------------------------------------------------------------------------
// The location abstraction: an in-bound x-register or an addressable Y slot
// ---------------------------------------------------------------------------
//
// A `Y` operand is a slot in the current frame's Y area, so — unlike an x-register —
// it must be addressed through the guarded frame window (above). The aggregate
// readers/writer, the `swap` and the `init_yregs` lowering are all expressed over this
// one abstraction, so each of them emits the **same** guard the arithmetic fast path
// and a `Y`-operand `move` already use: a guard can only ever *reject* (branching to
// `jit_step_one`), so an in-frame `Y` access runs natively and a malformed frame keeps
// the interpreter's exact error. A `Loc::Y` is a stack slot, **never** an x-register:
// it is never charged to the register cache or the x-register usage counts.

/// The first caller-saved scratch a widened lowering holds a pre-computed `Y`
/// destination address in.
///
/// The guarded address computation needs the slot address and the window bound live at
/// once, and a lowering with two `Y` destinations (`get_list`'s head and tail) keeps
/// **both** addresses live across its store phase, so it needs two: this one and
/// [`Y_ADDR_SCRATCH_2`]. Neither is a cache register ([`super::CACHE_REGISTERS`]) nor
/// the block's reserved `rbx`/`rsp`.
pub(super) const Y_ADDR_SCRATCH: Reg = Reg::R9;
/// The second pre-computed `Y` destination address scratch (see [`Y_ADDR_SCRATCH`]).
pub(super) const Y_ADDR_SCRATCH_2: Reg = Reg::R8;

/// A place a natively lowered instruction reads from or writes to: an x-register
/// inside the engine's [`MAX_REGISTER`] bound, or a `Y` slot the emitted `disp32` can
/// address ([`y_addressable`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Loc {
    /// An x-register inside the engine's [`MAX_REGISTER`] bound.
    X(u32),
    /// An addressable `Y` slot, accessed through the guarded current-frame window.
    Y(u32),
}

impl Loc {
    /// The [`Loc`] an operand names, or `None` when it is neither an in-bound
    /// x-register nor an addressable `Y` slot.
    ///
    /// Purely syntactic (it never consults the register allocation), so the emitter and
    /// [`super::regalloc::allocate_registers`] derive the same answer and agree on which
    /// instructions are lowered.
    pub(super) fn of(operand: &Operand) -> Option<Loc> {
        if let Some(x) = x_register(operand) {
            return (x < MAX_REGISTER).then_some(Loc::X(x));
        }
        if let Some(k) = y_register(operand) {
            return y_addressable(k).then_some(Loc::Y(k));
        }
        None
    }

    /// Whether this location is a `Y` slot — i.e. whether reading/writing it touches the
    /// current frame's Y window ([`super::arith::uses_y`]'s per-shape test).
    pub(super) fn is_y(self) -> bool {
        matches!(self, Loc::Y(_))
    }

    /// The x-register index this location names, if any: a `Y` slot is a stack slot, so
    /// it contributes nothing to a shape's x-register set.
    pub(super) fn register(self) -> Option<u32> {
        match self {
            Loc::X(x) => Some(x),
            Loc::Y(_) => None,
        }
    }

    /// Read this location into `dst`.
    ///
    /// An x-register reads its cache register or its `machine.x` slot (recording the
    /// direct access); a `Y` slot is the guarded read ([`emit_guarded_y_load`]), whose
    /// rejection jumps to `fallback` **before** anything is written. `end` is the
    /// window-bound scratch the `Y` read needs and must differ from `dst` (it is unused
    /// for an x-register).
    pub(super) fn load(
        self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        dst: Reg,
        end: Reg,
        fallback: CodeLabel,
        highest_touched: &mut Option<u32>,
    ) -> Result<()> {
        match self {
            Loc::X(x) => match allocation.slot(x) {
                Some(reg) => buffer.mov_rr(dst, reg),
                None => {
                    buffer.mov_rm(dst, Reg::Rbx, x_offset(x))?;
                    touch(highest_touched, x);
                    Ok(())
                }
            },
            Loc::Y(k) => emit_guarded_y_load(buffer, dst, end, k, fallback),
        }
    }

    /// Emit this location's frame-window guard, leaving a `Y` slot's address in `addr`;
    /// an x-register emits nothing.
    ///
    /// This is the "guard phase" of a lowering that writes **more than one**
    /// destination: every `Y` guard is emitted (and may reject) **before** any store, so
    /// a rejection can never leave a partially written destination behind. `end` is the
    /// window-bound scratch and must differ from `addr`.
    pub(super) fn guard_store(
        self,
        buffer: &mut CodeBuffer,
        addr: Reg,
        end: Reg,
        fallback: CodeLabel,
    ) -> Result<()> {
        match self {
            Loc::X(_) => Ok(()),
            Loc::Y(k) => guard_y_store(buffer, k, addr, end, fallback),
        }
    }

    /// Write `src` into this location, using the `Y` address `addr` a preceding
    /// [`Loc::guard_store`] produced (a no-op for an x-register).
    ///
    /// An x-register writes into its cache register or its `machine.x` slot (the very
    /// slot the exit spill writes), recording the direct access. This emits **no** guard:
    /// the caller must have emitted it ([`Loc::guard_store`], or the combined
    /// [`emit_guarded_y_store`] on a path that stores exactly one destination).
    pub(super) fn store_at(
        self,
        buffer: &mut CodeBuffer,
        addr: Reg,
        src: Reg,
        allocation: &Allocation,
        highest_touched: &mut Option<u32>,
    ) -> Result<()> {
        match self {
            Loc::X(x) => match allocation.slot(x) {
                Some(dst) => buffer.mov_rr(dst, src),
                None => {
                    buffer.mov_mr(Reg::Rbx, x_offset(x), src)?;
                    touch(highest_touched, x);
                    Ok(())
                }
            },
            // The guard was emitted (and recorded) by `guard_store`; this is its store.
            Loc::Y(_) => buffer.mov_mr(addr, 0, src),
        }
    }
}

/// The [`Loc`] of every operand of `operands`, or `None` when **any** of them is
/// neither an in-bound x-register nor an addressable `Y` slot.
///
/// The all-or-nothing predicate a **whole-list** lowering (`init_yregs`) rests on: an
/// operand kind the tier cannot address (a float register, a literal, an out-of-bound
/// index, an un-addressable `Y` index) refuses the whole instruction at compile time, so
/// the caller emits exactly today's `jit_step_one` step and the interpreter reports its
/// faithful error. An empty list yields an empty vector (a no-op instruction, lowered
/// exactly).
pub(super) fn locate_all(operands: &[Operand]) -> Option<Vec<Loc>> {
    operands.iter().map(Loc::of).collect()
}
