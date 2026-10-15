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

//! The baseline JIT's inline `move`/`swap`: shuffling a value between an x-register
//! (its machine register when the allocator caches it, else its `machine.x` slot), a
//! `Y` slot behind the shared frame-window guard, or an immediate constant.

use beam_common::Result;
use beam_loader::Operand;
use beam_term::Eterm;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::operands::{
    constant_word, emit_guarded_y_load, emit_guarded_y_store, x_offset, x_register, y_addressable,
    y_register, Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2,
};
use super::super::runtime::{RUNTIME_LITERALS_DISP, RUNTIME_LITERALS_END_DISP, RUNTIME_SLOT_DISP};
use super::{emit_step_and_continue, touch, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};
use std::mem;

/// The third caller-saved scratch a Y-operand `move` needs.
///
/// The guarded Y address computation holds the slot address **and** the window bound at
/// once, and a Y-destination store holds the value being transferred as well: three
/// registers. Two are [`MOVE_SCRATCH`]/[`MOVE_SCRATCH_2`]; this is the third. It is
/// used strictly *within* one instruction's lowering — no value survives into the next
/// instruction, and no helper call happens in between — so any caller-saved register
/// works; this one is neither a cache register ([`super::CACHE_REGISTERS`]) nor the
/// block's reserved `rbx` (the `machine.x` base) / `rsp` (the `rt` slot).
const Y_SCRATCH: Reg = Reg::R9;

/// The caller-saved register a `Y`-slot **exchange** uses as the frame-window bound
/// scratch of its guards.
///
/// A `swap` holds *two* values live at once ([`MOVE_SCRATCH`]/[`MOVE_SCRATCH_2`]) and up
/// to two `Y` slot addresses ([`Y_ADDR_SCRATCH`]/[`Y_ADDR_SCRATCH_2`]), so the guard's
/// window bound needs a fifth scratch; `rax` is caller-saved and used strictly within one
/// instruction's lowering (the same role it plays for the arithmetic fast path). It is
/// neither a cache register ([`super::CACHE_REGISTERS`]) nor the block's reserved
/// `rbx`/`rsp`.
const SWAP_END: Reg = Reg::Rax;

/// Where a `move`'s source value comes from, syntactically.
#[derive(Clone, Copy)]
enum MoveValue {
    /// An x-register inside the engine's [`MAX_REGISTER`] bound.
    X(u32),
    /// A `Y` slot the emitted `disp32` can address, read through the guarded
    /// current-frame window.
    Y(u32),
    /// A constant operand, materialised as its raw `Eterm` word.
    Constant(i64),
    /// A static literal, by index into the module's literal area
    /// (`Operand::Literal(n)`): loaded **at run time** from the base the driver
    /// publishes ([`super::super::runtime::JitRuntime::refresh_literals`]) — never
    /// baked into the emitted code as an immediate (a cached artifact outlives the
    /// particular `Arc<Module>` it was compiled for; see the field docs).
    Literal(u32),
}

/// Where a `move`'s value is written, syntactically.
#[derive(Clone, Copy)]
enum MoveDest {
    /// An x-register inside the engine's [`MAX_REGISTER`] bound.
    X(u32),
    /// An addressable `Y` slot, written through the guarded current-frame window.
    Y(u32),
}

/// The inlinable shape of a `move`, or `None` when the whole instruction must stay on
/// the helper.
///
/// Purely syntactic (it never consults the register allocation, so
/// [`super::regalloc::allocate_registers`] and [`super::emit_block`] agree on which
/// moves are lowered): the destination must be an in-bound x-register or an
/// addressable `Y` slot, and the source an in-bound x-register, an addressable `Y`
/// slot, or a constant word. A destination or source outside that subset (`fr`, a
/// literal, an x-register past [`MAX_REGISTER`], an un-addressable `Y` index) is the
/// documented deopt — the helper reports the interpreter's exact result, including its
/// faithful `bad_register` error.
pub(crate) struct MoveShape {
    /// The source value's location.
    source: MoveValue,
    /// The destination's location.
    destination: MoveDest,
}

impl MoveShape {
    /// The x-register indices the shape's native lowering touches, in emission order
    /// (the source first, then the destination).
    ///
    /// A `Y` slot is **not** an x-register, so it is never charged: raising the
    /// x-register bound the driver's entry guard reads for a stack slot would wrongly
    /// refuse native entry.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        if let MoveValue::X(x) = self.source {
            registers.push(x);
        }
        if let MoveDest::X(x) = self.destination {
            registers.push(x);
        }
        registers
    }

    /// The x-register indices the shape's native lowering **writes** — the
    /// destination register when it is an x-register, else nothing (a `Y`
    /// destination is a stack slot, never a cached register).
    ///
    /// This is the block register cache's write set ([`super::Allocation::written`]):
    /// a cached register outside it keeps exactly its `machine.x` value, so the exit
    /// spill may skip it.
    pub(crate) fn writes(&self) -> Vec<u32> {
        match self.destination {
            MoveDest::X(x) => vec![x],
            MoveDest::Y(_) => Vec::new(),
        }
    }

    /// Whether the lowered sequence reads the current frame's Y window.
    ///
    /// A `Y` source or a `Y` destination is read/written through
    /// `JitRuntime::y_base`/`y_end`, so the emitted code needs the window published.
    /// This is the gate [`super::arith::uses_y`] consults for a `move` (see that
    /// function for why the two must agree exactly).
    pub(crate) fn uses_y(&self) -> bool {
        matches!(self.source, MoveValue::Y(_)) || matches!(self.destination, MoveDest::Y(_))
    }
}

/// The inlinable shape of a `move`, or `None` when the whole instruction must stay on
/// the helper (see [`MoveShape`]).
pub(crate) fn move_shape(src: &Operand, dst: &Operand) -> Option<MoveShape> {
    let destination = if let Some(x) = x_register(dst) {
        if x >= MAX_REGISTER {
            // `machine.x` is at most `MAX_REGISTER` long, so addressing `x` directly
            // would be out of bounds; the helper reports the faithful `bad_register`.
            return None;
        }
        MoveDest::X(x)
    } else if let Some(k) = y_register(dst) {
        if !y_addressable(k) {
            return None;
        }
        MoveDest::Y(k)
    } else {
        return None;
    };

    let source = if let Some(x) = x_register(src) {
        if x >= MAX_REGISTER {
            return None;
        }
        MoveValue::X(x)
    } else if let Some(k) = y_register(src) {
        if !y_addressable(k) {
            return None;
        }
        MoveValue::Y(k)
    } else if let Operand::Literal(index) = src {
        // A **literal-area** source. Unlike the other three kinds, whether it is
        // inlinable does not depend on the *module* at all: the word is loaded from
        // the area the driver publishes at run time, and an absent area (or an index
        // out of it) is a **run-time guard rejection**, not a compile-time refusal —
        // which keeps `move_shape` module-independent, exactly as
        // `super::regalloc::cacheable_touches` needs it to be. The one compile-time
        // refusal left is the `disp32` the load's addressing carries.
        if !literal_addressable(*index) {
            return None;
        }
        MoveValue::Literal(*index)
    } else {
        MoveValue::Constant(constant_word(src)?)
    };

    Some(MoveShape {
        source,
        destination,
    })
}

/// Where an x-only `move`'s source value comes from.
enum MoveSource {
    /// An x-register the block holds in a machine register.
    Cached(Reg),
    /// An x-register the block does **not** cache — read straight from its
    /// `machine.x` slot (`[rbx + x*8]`), which is where the value is at a block
    /// boundary.
    Memory(u32),
    /// A constant operand, materialised as its raw `Eterm` word.
    Constant(i64),
}

/// Emit `move Src Dst` natively, or report that the block must helper it.
///
/// Returns `true` when the move was emitted. The inlinable shapes are those of
/// [`move_shape`]:
///
/// * an **x-only** move (a source and a destination that both name x-registers or a
///   constant) is emitted as the direct register/`machine.x` shuffle it always was —
///   `mov` between the two representations, with no run-time guard;
/// * a move whose **source and/or destination is an addressable `Y` slot** is emitted
///   behind the shared frame-window guard ([`emit_guarded_y_load`] /
///   [`emit_guarded_y_store`]), mirroring the arithmetic fast path: the emitted code
///   computes the slot address, compares it against `y_end`, and branches to the
///   `jit_step_one` fallback when the slot lies outside the current frame — so an
///   in-frame `Y` move runs natively and a malformed frame keeps the interpreter's exact
///   error. Nothing is committed before a guard can reject, so a rejected guard leaves
///   the machine exactly as the interpreter will re-run it.
///
/// A destination that is neither an in-bound x-register nor an addressable `Y` slot, a
/// source that is none of the three, or an operand index past its bound, still deopts
/// to the helper — which reports the interpreter's exact result (including its faithful
/// `bad_register` error).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_move(
    buffer: &mut CodeBuffer,
    src: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = move_shape(src, dst) else {
        return Ok(false);
    };

    // A literal-area source: the word is loaded at run time from the area the driver
    // published (see [`emit_move_literal`]) behind the literal guard. This comes first
    // because it handles **both** destination kinds — an in-bound x-register and an
    // addressable `Y` slot — through the machinery the other lowerings already use.
    if let MoveValue::Literal(index) = shape.source {
        emit_move_literal(
            buffer,
            &shape,
            index,
            allocation,
            done,
            func,
            pc,
            highest_touched,
        )?;
        return Ok(true);
    }

    if shape.uses_y() {
        emit_move_y(
            buffer,
            &shape,
            allocation,
            done,
            func,
            pc,
            highest_touched,
        )?;
        return Ok(true);
    }

    // X-only: the direct shuffle. `uses_y()` is false, so the destination is an
    // x-register and the source an x-register or a constant; anything else (which the
    // shape cannot produce) refuses the lowering rather than fabricating a store.
    let MoveDest::X(dst_x) = shape.destination else {
        return Ok(false);
    };
    let source = match shape.source {
        MoveValue::X(src_x) => match allocation.slot(src_x) {
            Some(reg) => MoveSource::Cached(reg),
            None => MoveSource::Memory(src_x),
        },
        MoveValue::Constant(word) => MoveSource::Constant(word),
        // Unreachable: a literal source is lowered by `emit_move_literal` before this
        // arm is reached (it handles both destination kinds). Kept as a refusal rather
        // than a panic so a hypothetical future path deopts instead of mis-emitting.
        MoveValue::Y(_) | MoveValue::Literal(_) => return Ok(false),
    };

    match (allocation.slot(dst_x), source) {
        // Cached destination: write its machine register.
        (Some(dst_reg), MoveSource::Cached(src_reg)) => buffer.mov_rr(dst_reg, src_reg)?,
        (Some(dst_reg), MoveSource::Memory(src_x)) => {
            buffer.mov_rm(dst_reg, Reg::Rbx, x_offset(src_x))?;
            touch(highest_touched, src_x);
        }
        (Some(dst_reg), MoveSource::Constant(word)) => buffer.mov_ri(dst_reg, word)?,
        // Uncached destination: write its `machine.x` slot directly.
        (None, MoveSource::Cached(src_reg)) => {
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), src_reg)?;
        }
        (None, MoveSource::Memory(src_x)) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(src_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?;
            touch(highest_touched, src_x);
        }
        (None, MoveSource::Constant(word)) => {
            buffer.mov_ri(MOVE_SCRATCH, word)?;
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?;
        }
    }
    touch(highest_touched, dst_x);
    Ok(true)
}

/// Lower a `move` whose source and/or destination is an addressable `Y` slot (see
/// [`try_inline_move`]). The emitted shape:
///
/// ```text
///   <load the source into MOVE_SCRATCH>
///     ; an x-register: its cache register, or `mov MOVE_SCRATCH, [rbx + x*8]`
///     ; a constant:    `movabs MOVE_SCRATCH, word`
///     ; a Y slot:      the guarded Y read (Y_SCRATCH holds `y_end`)
///   <store MOVE_SCRATCH into the destination>
///     ; an x-register: its cache register, or `mov [rbx + x*8], MOVE_SCRATCH`
///     ; a Y slot:      the guarded Y store (MOVE_SCRATCH_2 = the slot address,
///     ;                Y_SCRATCH = `y_end`, MOVE_SCRATCH = the value)
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// Both Y guards reject to the **same** `fallback`, and neither is reached after any
/// state has been committed: the source is read into a scratch and the destination is
/// written once, so a rejected guard (an out-of-frame slot, or no live frame) leaves
/// the machine exactly as it was — the interpreter then re-runs the whole instruction.
/// A Y slot is never charged to `highest_touched` (it is not an x-register); the
/// x-register operands are, exactly as [`MoveShape::registers`] lists them.
#[allow(clippy::too_many_arguments)]
fn emit_move_y(
    buffer: &mut CodeBuffer,
    shape: &MoveShape,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // The source into the value scratch. A Y source carries its own frame-window
    // guard, whose rejection jumps to `fallback` before anything is written.
    match shape.source {
        MoveValue::X(src_x) => match allocation.slot(src_x) {
            Some(reg) => buffer.mov_rr(MOVE_SCRATCH, reg)?,
            None => {
                buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(src_x))?;
                touch(highest_touched, src_x);
            }
        },
        MoveValue::Y(k) => emit_guarded_y_load(buffer, MOVE_SCRATCH, Y_SCRATCH, k, fallback)?,
        // Unreachable from `try_inline_move` (a literal source takes `emit_move_literal`),
        // but emitted in full rather than refused: the load is the same guarded read into
        // the value scratch, so this arm is correct even if a future path reaches it.
        MoveValue::Literal(index) => emit_guarded_literal_load(
            buffer,
            MOVE_SCRATCH,
            MOVE_SCRATCH_2,
            Y_SCRATCH,
            index,
            fallback,
        )?,
        MoveValue::Constant(word) => buffer.mov_ri(MOVE_SCRATCH, word)?,
    }

    // The destination, from the value scratch. A Y destination carries the same
    // guard; a rejection still commits nothing (the value lives only in a scratch).
    match shape.destination {
        MoveDest::X(dst_x) => match allocation.slot(dst_x) {
            Some(reg) => buffer.mov_rr(reg, MOVE_SCRATCH)?,
            None => {
                buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?;
                touch(highest_touched, dst_x);
            }
        },
        MoveDest::Y(k) => {
            emit_guarded_y_store(buffer, MOVE_SCRATCH, MOVE_SCRATCH_2, Y_SCRATCH, k, fallback)?
        }
    }
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}

/// Whether a literal index is addressable as the `disp32` of the emitted load.
///
/// `y_addressable`'s twin for the literal area: a `disp32` cannot carry an offset past
/// `i32::MAX`, so an index whose byte offset would not fit is refused at **compile**
/// time (a deopt is always correct — the interpreter runs the instruction). Reachable
/// only for a malformed module: `LitT`'s payloads are bounded by the chunk size, so a
/// real literal index is orders of magnitude below the bound.
fn literal_addressable(index: u32) -> bool {
    (index as u64)
        .checked_mul(mem::size_of::<Eterm>() as u64)
        .is_some_and(|offset| offset <= i32::MAX as u64)
}

/// The byte offset of literal `index` inside the published area — the displacement the
/// emitted `lea [base + index*8]` carries. `index` must satisfy [`literal_addressable`].
fn literal_offset(index: u32) -> i32 {
    (index as i32) * (mem::size_of::<Eterm>() as i32)
}

/// Emit the **guarded** load of literal `index` into `value`, jumping to `fallback`
/// when the index lies outside the literal area the driver published.
///
/// This is the literal-area twin of [`emit_guarded_y_address`], in the identical
/// shape and with the identical "null means reject every access" discipline:
///
/// ```text
///   mov  addr, [rsp + RUNTIME_SLOT_DISP]        ; rt (the block's own stack slot)
///   mov  end,  [addr + RUNTIME_LITERALS_END_DISP]
///   mov  addr, [addr + RUNTIME_LITERALS_DISP]   ; the area's base
///   lea  addr, [addr + index*8]                 ; &area[index]
///   cmp  addr, end
///   jae  fallback                               ; no area, or index >= len
///   mov  value, [addr]                          ; the literal word, untouched
/// ```
///
/// The guard can only ever **reject**, so an absent area (base and end both null, which
/// makes `index*8 >= 0` true for every index) or an out-of-area index costs exactly the
/// `jit_step_one` step it cost before this lowering existed — and the interpreter then
/// reports its own exact result, including its faithful out-of-range behaviour. The
/// loaded word is copied verbatim: it stays a pointer into the shared, read-only literal
/// area, never rebuilt and never copied.
///
/// `value`, `addr` and `end` must be three distinct caller-saved scratch registers that
/// are dead at the access; none may be a cache register ([`super::CACHE_REGISTERS`]) or
/// the block's reserved `rbx`/`rsp`.
fn emit_guarded_literal_load(
    buffer: &mut CodeBuffer,
    value: Reg,
    addr: Reg,
    end: Reg,
    index: u32,
    fallback: CodeLabel,
) -> Result<()> {
    buffer.mov_rm(addr, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_rm(end, addr, RUNTIME_LITERALS_END_DISP)?;
    buffer.mov_rm(addr, addr, RUNTIME_LITERALS_DISP)?;
    buffer.lea(addr, addr, literal_offset(index))?;
    buffer.cmp_rr(addr, end)?;
    buffer.jcc(Cond::Ae, fallback)?;
    buffer.mov_rm(value, addr, 0)
}

/// Lower a `move` whose **source is a literal-area index** (see [`try_inline_move`]).
/// The emitted shape:
///
/// ```text
///   <the guarded literal load into MOVE_SCRATCH>   ; [base + index*8], behind the
///                                                  ;   area guard (MOVE_SCRATCH_2 =
///                                                  ;   the address, Y_SCRATCH = the
///                                                  ;   published area end)
///   <store MOVE_SCRATCH into the destination>
///     ; an x-register: its cache register, or `mov [rbx + x*8], MOVE_SCRATCH`
///     ; a Y slot:      the guarded Y store (see `emit_move_y`)
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// Nothing is committed before a guard can reject (the word lives only in
/// `MOVE_SCRATCH`), so a rejection leaves the machine exactly as the interpreter will
/// re-run it. `MOVE_SCRATCH_2`/`Y_SCRATCH` are dead after the load, so the Y store
/// below reuses them as its own address/bound scratch. A `Y` destination is never
/// charged to `highest_touched` (it is not an x-register); an x-register destination is,
/// exactly as [`MoveShape::registers`] lists it.
#[allow(clippy::too_many_arguments)]
fn emit_move_literal(
    buffer: &mut CodeBuffer,
    shape: &MoveShape,
    index: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    emit_guarded_literal_load(
        buffer,
        MOVE_SCRATCH,
        MOVE_SCRATCH_2,
        Y_SCRATCH,
        index,
        fallback,
    )?;

    match shape.destination {
        MoveDest::X(dst_x) => {
            match allocation.slot(dst_x) {
                Some(reg) => buffer.mov_rr(reg, MOVE_SCRATCH)?,
                None => buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?,
            }
            touch(highest_touched, dst_x);
        }
        MoveDest::Y(k) => {
            emit_guarded_y_store(buffer, MOVE_SCRATCH, MOVE_SCRATCH_2, Y_SCRATCH, k, fallback)?
        }
    }
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}

/// The inlinable shape of a `swap` the tier lowers natively: the two locations the
/// exchange reads and writes.
///
/// Purely syntactic (it never consults the register allocation, so
/// [`super::regalloc::allocate_registers`] and [`super::emit_block`] agree on which
/// swaps are lowered): each operand must be an in-bound x-register or an addressable `Y`
/// slot ([`Loc`]).
pub(crate) struct SwapShape {
    /// The `a` operand's location.
    a: Loc,
    /// The `b` operand's location.
    b: Loc,
}

impl SwapShape {
    /// The x-register indices the exchange touches, in emission order (`a`, then `b`).
    ///
    /// A `Y` slot is **not** an x-register, so it is never charged: raising the
    /// x-register bound the driver's entry guard reads for a stack slot would wrongly
    /// refuse native entry.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.a.register());
        registers.extend(self.b.register());
        registers
    }

    /// The x-register indices the exchange **writes** — both operands when they are
    /// x-registers (the shuffle writes `machine.x` slots / the cache registers directly).
    /// A `Y` side is a stack slot, never a cached register.
    pub(crate) fn writes(&self) -> Vec<u32> {
        let mut writes = Vec::with_capacity(2);
        writes.extend(self.a.register());
        writes.extend(self.b.register());
        writes
    }

    /// Whether the lowered exchange reads or writes the current frame's Y window (a `Y`
    /// side), i.e. whether [`super::arith::uses_y`] must report this instruction so the
    /// window is published in the prologue and refreshed after an in-block helper.
    pub(crate) fn uses_y(&self) -> bool {
        self.a.is_y() || self.b.is_y()
    }
}

/// The inlinable shape of a `swap`, or `None` when the whole instruction must stay on the
/// helper (see [`SwapShape`]).
pub(crate) fn swap_shape(a: &Operand, b: &Operand) -> Option<SwapShape> {
    Some(SwapShape {
        a: Loc::of(a)?,
        b: Loc::of(b)?,
    })
}

/// Emit `swap A B` natively, or report that the block must helper it.
///
/// Returns `true` when the exchange was emitted for the shape [`swap_shape`] claims.
/// A **`Y`-free** shape (both operands in-bound x-registers) is the direct
/// register/`machine.x` shuffle it always was — each operand read and written through its
/// own representation, so the exchange is exact for all four cache combinations and
/// **byte-identical** to the pre-widening emission. A shape with a `Y` side goes through
/// [`emit_swap_y`] behind the shared frame-window guard: an in-frame `Y` exchange runs
/// natively, and a malformed frame keeps the interpreter's exact error. Anything
/// [`swap_shape`] refuses (a float register, a literal, an out-of-bound index, an
/// un-addressable `Y` index) deopts, and the helper performs the interpreter's exact
/// `va = read(a); vb = read(b); write(a, vb); write(b, va)` exchange.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_swap(
    buffer: &mut CodeBuffer,
    a: &Operand,
    b: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = swap_shape(a, b) else {
        return Ok(false);
    };
    if shape.uses_y() {
        emit_swap_y(buffer, &shape, allocation, done, func, pc, highest_touched)?;
        return Ok(true);
    }
    let (Loc::X(a_x), Loc::X(b_x)) = (shape.a, shape.b) else {
        return Ok(false);
    };
    match (allocation.slot(a_x), allocation.slot(b_x)) {
        (Some(a_reg), Some(b_reg)) => {
            // `xchg` spelled as three moves through the scratch register (the
            // encoder has no `xchg`, and a register pair that overlaps the
            // destination is not expressible with `mov` alone otherwise).
            buffer.mov_rr(MOVE_SCRATCH, a_reg)?;
            buffer.mov_rr(a_reg, b_reg)?;
            buffer.mov_rr(b_reg, MOVE_SCRATCH)?;
        }
        (Some(a_reg), None) => {
            // a is cached, b lives in `machine.x`.
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(b_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(b_x), a_reg)?;
            buffer.mov_rr(a_reg, MOVE_SCRATCH)?;
            touch(highest_touched, b_x);
        }
        (None, Some(b_reg)) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(a_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(a_x), b_reg)?;
            buffer.mov_rr(b_reg, MOVE_SCRATCH)?;
            touch(highest_touched, a_x);
        }
        (None, None) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(a_x))?;
            buffer.mov_rm(MOVE_SCRATCH_2, Reg::Rbx, x_offset(b_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(a_x), MOVE_SCRATCH_2)?;
            buffer.mov_mr(Reg::Rbx, x_offset(b_x), MOVE_SCRATCH)?;
            touch(highest_touched, a_x);
            touch(highest_touched, b_x);
        }
    }
    Ok(true)
}

/// Lower a `swap` with an addressable `Y` side through the shared frame-window guard
/// (see [`try_inline_swap`]). The emitted shape:
///
/// ```text
///   <read both operands>    ; va -> MOVE_SCRATCH, vb -> MOVE_SCRATCH_2, in the
///                           ;   interpreter's own `read(a)`-then-`read(b)` order, so an
///                           ;   exchange whose sides alias still ends with the
///                           ;   interpreter's values (a Y read is the guarded load)
///   <guard phase>           ; every Y side's window guard, leaving its slot address in
///                           ;   Y_ADDR_SCRATCH / Y_ADDR_SCRATCH_2 — BEFORE any store, so
///                           ;   a rejection can never leave one side exchanged and the
///                           ;   other not
///   <store phase>           ; write(a, vb); write(b, va)
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// Both values live in scratch across the whole sequence (a value is never read from a
/// destination), so the alias-safe read-both-then-write-both order of the interpreter is
/// reproduced exactly for every combination of x-register (`machine.x` slot or cache
/// register) and `Y` slot. A `Y` slot is never charged to `highest_touched` (it is not an
/// x-register); the x-register sides are, exactly as [`SwapShape::registers`] lists them.
#[allow(clippy::too_many_arguments)]
fn emit_swap_y(
    buffer: &mut CodeBuffer,
    shape: &SwapShape,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // `va = read(a); vb = read(b)` — both before any store.
    shape
        .a
        .load(buffer, allocation, MOVE_SCRATCH, SWAP_END, fallback, highest_touched)?;
    shape.b.load(
        buffer,
        allocation,
        MOVE_SCRATCH_2,
        SWAP_END,
        fallback,
        highest_touched,
    )?;

    // The guard phase: each `Y` side's window guard, both before the first store.
    if shape.a.is_y() {
        shape
            .a
            .guard_store(buffer, Y_ADDR_SCRATCH, SWAP_END, fallback)?;
    }
    if shape.b.is_y() {
        shape
            .b
            .guard_store(buffer, Y_ADDR_SCRATCH_2, SWAP_END, fallback)?;
    }

    // `write(a, vb); write(b, va)` — the interpreter's order.
    shape
        .a
        .store_at(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, allocation, highest_touched)?;
    shape
        .b
        .store_at(buffer, Y_ADDR_SCRATCH_2, MOVE_SCRATCH, allocation, highest_touched)?;
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}
