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

//! The baseline JIT's guarded cons-cell readers — the `get_list`/`get_hd`/`get_tl`
//! lowerings behind the `is_list` guard, which can only ever *reject* (a non-cons
//! source falls back to `jit_step_one`, whose interpreter step raises the exact
//! `{badmatch, Source}`). The parent `aggregate` module owns the tuple readers and the
//! tuple writer and re-exports this module's shapes, so `emit::aggregate::…` keeps
//! resolving for every caller in this directory.

use std::mem::size_of;

use beam_common::Result;
use beam_loader::Operand;
use beam_term::{Eterm, CAR_OFFSET, CDR_OFFSET, TAG_PRIMARY_LIST, TAG_PRIMARY_MASK, TAG_PTR_MASK};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::operands::{Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::super::{emit_step_and_continue, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};

// ---------------------------------------------------------------------------
// The cons-cell readers
// ---------------------------------------------------------------------------
//
// `get_list`/`get_hd`/`get_tl` split a cons cell (`emu/instrs.tab` `get_list`
// `:389`, `get_hd` `:398`, `get_tl` `:403`). The interpreter
// (`crate::families::lists`) reads the source, gates on `beam_term::is_list`
// (via `terms::cons_head`/`cons_tail`) and, on success, writes the CAR — then
// the CDR — to the destination operands; a non-cons source *raises*
// `{badmatch, Source}` through `terms::badmatch_reason`.
//
// The subset below is lowered natively and mirrors that branch for branch:
//
// * the source and **every** destination must be an in-bound x-register or an
//   addressable `Y` slot ([`Loc`]): a register index past the bound would read out
//   of `machine.x`, and a `Y` slot is addressed through the guarded frame window —
//   both refused kinds deopt, so the interpreter step reports the faithful error;
// * the guard is `is_list` itself, the C bit trick
//   `(word & (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) == 0`: the emitted `and`
//   leaves `0` for a cons cell (primary tag `LIST`) and the mask itself for
//   everything else — `NIL`, a small int, an atom, a boxed term (`[]`, `1`,
//   `foo`, a tuple, …) — so the comparison against the mask is exactly
//   "not a cons cell" and branches to the helper, whose interpreter step raises
//   the exact `{badmatch, Source}`;
// * on success the cell's address is `list_val(word)` (`word & !TAG_PTR_MASK`),
//   the CAR is the word at offset 0 and the CDR the word at the cons cell's
//   second word (`beam_term::{CAR_OFFSET, CDR_OFFSET}`).
//
// The destinations are written **CAR first, then CDR** — the interpreter's write
// order — so a list read whose destinations alias each other (or the source)
// ends with the CDR value, exactly as the interpreter's does.
//
// A `Y` destination's frame-window guard is evaluated **before any store** (the
// guards first, then the stores): the fallback re-runs the whole instruction in the
// interpreter, so a guard that rejected *after* an earlier store had already
// clobbered an operand the interpreter then re-reads (a destination that aliases the
// source) would diverge — front-loading every guard makes a rejection leave the
// machine exactly as the interpreter will re-run it.

/// `TAG_PRIMARY_MASK - TAG_PRIMARY_LIST` as an `i64` — the mask of the
/// `is_not_list` bit trick (`beam_term::list::is_not_list`), which is `0x2`.
const NOT_LIST_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST) as i64;

/// `!TAG_PTR_MASK` as an `i64` — the mask `list_val` clears the pointer tag with.
pub(crate) const POINTER_MASK: i64 = !(TAG_PTR_MASK as i64);

/// The byte offset of the CAR (head) word inside a cons cell.
const CAR_DISP: i32 = (size_of::<Eterm>() * CAR_OFFSET) as i32;

/// The byte offset of the CDR (tail) word inside a cons cell.
const CDR_DISP: i32 = (size_of::<Eterm>() * CDR_OFFSET) as i32;

/// Which half of a cons cell a destination receives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Half {
    /// The CAR (head) word — `get_list`'s head destination, `get_hd`.
    Head,
    /// The CDR (tail) word — `get_list`'s tail destination, `get_tl`.
    Tail,
}

/// The operands of a list read the tier lowers natively: the source cons cell and the
/// destinations it writes, in emission order.
#[derive(Clone)]
pub(crate) struct ListShape {
    /// The `source` operand's location.
    source: Loc,
    /// The `(half, destination)` writes, in emission order: the CAR before the CDR
    /// wherever both are written (`get_list`), so aliasing destinations end with the
    /// CDR value exactly as the interpreter's write order does.
    destinations: Vec<(Half, Loc)>,
}

impl ListShape {
    /// The x-register indices the shape's native lowering touches, in emission
    /// order (the source first, then each destination).
    ///
    /// A `Y` slot is **not** an x-register, so it is never charged: raising the
    /// x-register bound the driver's entry guard reads for a stack slot would wrongly
    /// refuse native entry.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(self.destinations.len() + 1);
        registers.extend(self.source.register());
        registers.extend(self.destinations.iter().filter_map(|(_, loc)| loc.register()));
        registers
    }

    /// The x-register indices the shape's native lowering **writes** — its
    /// destinations, in emission order (CAR before CDR wherever both are written).
    /// The source is only read, and a `Y` destination is not an x-register.
    ///
    /// This is the block register cache's write set ([`super::super::Allocation::written`]).
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.destinations.iter().filter_map(|(_, loc)| loc.register()).collect()
    }

    /// Whether the lowered sequence reads the current frame's Y window — a `Y` source
    /// or a `Y` destination ([`super::super::arith::uses_y`]'s per-shape test).
    pub(crate) fn uses_y(&self) -> bool {
        self.source.is_y() || self.destinations.iter().any(|(_, loc)| loc.is_y())
    }
}

/// The inlinable shape of a `get_list Source Head Tail`, or `None` when the whole
/// instruction must stay on the helper.
pub(crate) fn get_list_shape(
    source: &Operand,
    head: &Operand,
    tail: &Operand,
) -> Option<ListShape> {
    shape(source, &[(Half::Head, head), (Half::Tail, tail)])
}

/// The inlinable shape of a `get_hd Source Head`, or `None` when the whole
/// instruction must stay on the helper.
pub(crate) fn get_hd_shape(source: &Operand, head: &Operand) -> Option<ListShape> {
    shape(source, &[(Half::Head, head)])
}

/// The inlinable shape of a `get_tl Source Tail`, or `None` when the whole
/// instruction must stay on the helper.
pub(crate) fn get_tl_shape(source: &Operand, tail: &Operand) -> Option<ListShape> {
    shape(source, &[(Half::Tail, tail)])
}

/// The inlinable shape of a list read that writes `writes` (in that order), or
/// `None` for a shape above the subset.
///
/// The source and every destination must be an in-bound x-register or an addressable
/// `Y` slot — see the section comment above.
fn shape(source: &Operand, writes: &[(Half, &Operand)]) -> Option<ListShape> {
    let source = Loc::of(source)?;
    let destinations = writes
        .iter()
        .map(|(half, operand)| Loc::of(operand).map(|loc| (*half, loc)))
        .collect::<Option<Vec<_>>>()?;
    Some(ListShape {
        source,
        destinations,
    })
}

/// Emit `get_list Source Head Tail` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the CAR in
/// `head` and the CDR in `tail`. Returns `false` **without having emitted a byte**
/// when the operands are not three in-bound x-registers ([`get_list_shape`]) — the
/// caller then emits exactly today's `jit_step_one` step.
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_inline_get_list(
    buffer: &mut CodeBuffer,
    source: &Operand,
    head: &Operand,
    tail: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_list_shape(source, head, tail) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Emit `get_hd Source Head` natively, or report that the block must helper it
/// (see [`try_inline_get_list`] for the return convention).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_inline_get_hd(
    buffer: &mut CodeBuffer,
    source: &Operand,
    head: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_hd_shape(source, head) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Emit `get_tl Source Tail` natively, or report that the block must helper it
/// (see [`try_inline_get_list`] for the return convention).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_inline_get_tl(
    buffer: &mut CodeBuffer,
    source: &Operand,
    tail: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_tl_shape(source, tail) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Lower an inlinable list read (see the section comment above for why it is
/// exact). The emitted shape:
///
/// ```text
///   mov  r11, <source>      ; the source word: a cached register, `[rbx + x*8]`,
///                           ;   or a guarded Y read (r10 = the window bound)
///   mov  r10, NOT_LIST_MASK ; the `is_not_list` bit
///   and  r11, r10
///   cmp  r11, r10           ; the masked word is 0 (a cons cell) or the mask itself
///   je   fallback           ; not a cons cell: the helper raises `{badmatch,Source}`
///   mov  r11, <source>      ; reload (the mask consumed the word)
///   mov  r10, POINTER_MASK
///   and  r11, r10           ; r11 = the cons cell's address, `list_val(word)`
///   <guard phase>           ; each Y destination's window guard, leaving its slot
///                           ;   address in r9 / r8 — BEFORE any store, so a
///                           ;   rejection leaves no destination half-written
///   <store phase>           ; CAR first, then the CDR:
///   mov  r10, [r11 + 0]     ;   for a `get_list`, first ...
///   mov  <head>, r10        ;     <head> = a cache register, `[rbx + x*8]`, or `[r9]`
///   mov  r10, [r11 + 8]     ;   ... then the tail
///   mov  <tail>, r10
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// A `get_hd` writes only the CAR and a `get_tl` only the CDR (the half each
/// destination receives is [`ListShape::destinations`]' `Half`). Every destination
/// is written through its own representation — a cached register, its `machine.x`
/// slot (the very slot the exit spill would write), or a `Y` slot behind its guard —
/// and the source is read the same way, so the tier's liveness model is unchanged.
/// The fast path never spills.
fn emit_list_read(
    buffer: &mut CodeBuffer,
    shape: &ListShape,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // The guard: `is_not_list(source)` — a non-cons source (`[]`, a small int, an
    // atom, a tuple) branches to the helper, which raises `{badmatch, Source}`.
    shape
        .source
        .load(buffer, allocation, MOVE_SCRATCH, MOVE_SCRATCH_2, fallback, highest_touched)?;
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_LIST_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.cmp_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;

    // The cell's address: reload the source word (the mask above consumed it) and
    // clear the pointer tag — `list_val(word)`.
    shape
        .source
        .load(buffer, allocation, MOVE_SCRATCH, MOVE_SCRATCH_2, fallback, highest_touched)?;
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;

    // The **guard phase**: every `Y` destination's frame-window guard, leaving the slot
    // address in its own scratch. All of them run before the first store, so a rejection
    // can never leave a destination written for the interpreter's re-run to read
    // (a destination may alias the source). The two address scratches are enough:
    // `get_list` has at most two destinations.
    let mut y_addresses: Vec<(usize, Reg)> = Vec::new();
    for (index, (_, destination)) in shape.destinations.iter().enumerate() {
        if destination.is_y() {
            let addr = if y_addresses.is_empty() {
                Y_ADDR_SCRATCH
            } else {
                Y_ADDR_SCRATCH_2
            };
            destination.guard_store(buffer, addr, MOVE_SCRATCH_2, fallback)?;
            y_addresses.push((index, addr));
        }
    }

    // The **store phase**: CAR first, then CDR — the interpreter's write order, so
    // aliasing destinations (`head == tail`, or a destination that is the source) end
    // with the CDR value.
    for (index, (half, destination)) in shape.destinations.iter().enumerate() {
        let disp = match half {
            Half::Head => CAR_DISP,
            Half::Tail => CDR_DISP,
        };
        buffer.mov_rm(MOVE_SCRATCH_2, MOVE_SCRATCH, disp)?;
        let addr = y_addresses
            .iter()
            .find(|(i, _)| *i == index)
            .map_or(Y_ADDR_SCRATCH, |(_, addr)| *addr);
        destination.store_at(buffer, addr, MOVE_SCRATCH_2, allocation, highest_touched)?;
    }
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}
